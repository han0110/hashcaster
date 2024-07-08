use std::{mem::{transmute, MaybeUninit}, ops::Add, sync::atomic};

use num_traits::{One, Zero};
use crate::field::{pi, F128};
use itertools::Itertools;

pub fn eq_poly(pt: &[F128]) -> Vec<F128> {
    let l = pt.len();
    let mut ret = Vec::with_capacity(1 << l);
    ret.push(F128::one());
    for i in 0..l {
//        let pt_idx = l - i - 1;
        let half = 1 << i;
        for j in 0..half {
            ret.push(pt[i] * ret[j]);
        }
        for j in 0..half{
            let tmp = ret[half + j];
            ret[j] += tmp;
        }
    }
    ret
}

pub fn eq_ev(x: &[F128], y: &[F128]) -> F128 {
    x.iter().zip_eq(y.iter()).fold(F128::one(), |acc, (x, y)| acc * (F128::one() + x + y))
}

pub fn evaluate(poly: &[F128], pt: &[F128]) -> F128 {
    assert!(poly.len() == 1 << pt.len());
    poly.iter().zip_eq(eq_poly(pt)).fold(F128::zero(), |acc, (x, y)| acc + *x * y)
}

pub fn bits_to_trits(mut x: usize) -> usize {
    let mut multiplier = 1;
    let mut ret = 0;
    while x > 0 {
        ret += multiplier * (x % 2);
        x >>= 1;
        multiplier *= 3;
    }
    ret
}

/// Makes table 3^{c+1} * 2^{dims - c - 1}
pub fn extend_table(table: &[F128], dims: usize, c: usize) -> Vec<F128> {
    // TODO: as we have tested that this is correct, we should replace options by maybeuninits
    // TODO: suggest adding parallelization only on 2^k layer, as this algo for extension doesn't work well in parallel
    assert!(table.len() == 1 << dims);
    assert!(c < dims);
    let pow3 = 3usize.pow((c + 1) as u32);
    assert!(pow3 < (u16::MAX) as usize, "This is too large anyway ;)");
    let pow2 = 2usize.pow((dims - c - 1) as u32);

    let mut ret = vec![None; pow3 * pow2];
    for i in 0..(1 << dims) {
        let hi = i >> (c + 1);
        let lo = i ^ (hi << (c + 1));
        let j = bits_to_trits(lo) + hi * pow3;
        ret[j] = Some(table[i]);
    }

    for i in 0..(pow2 as u32) {
        for j in 0..(pow3 as u16) {
            // actual index: j + i * pow3
            let idx = (i as usize) * pow3 + j as usize;
            
            let mut counter = 0u32;
            let mut head = j;
            while head > 0 { // Search for the first trit = 2.
                let trit = head % 3;
                if trit == 2 {
                    let offset = 3u16.pow(counter) as usize;
                    assert!(ret[idx].is_none());
                    ret[idx] = Some(ret[idx - offset].unwrap() + ret[idx - 2*offset].unwrap());// set it 0 and 1, add values
                    break;
                }
                head /= 3;
                counter += 1;
            }
        };
    }

    ret.iter().map(|x|x.unwrap()).collect()
}

pub struct AndcheckProver {
    pt: Vec<F128>,
    p: Vec<F128>,
    q: Vec<F128>,
    c: usize, // PHASE SWITCH, round < c => PHASE 1.
    evaluation_claim: F128,
    challenges: Vec<F128>,
    p_q_ext: Option<Vec<F128>> // Table of evaluations on 3^{c+1-round} x 2^{n-c-1}
}

pub struct RoundResponse {
    pub values: Vec<F128>,
}

/// This struct holds evaluations of p and q in inverse Frobenius orbit of a challenge point.
pub struct FinalClaim {
    pub p_evs: Vec<F128>,
    pub q_evs: Vec<F128>,
}

impl FinalClaim {
    /// The function that computes evaluation of (P & Q) in a challenge point 
    /// through evaluations of P, Q in inverse Frobenius orbit.
    pub fn apply_algebraic_combinator(&self) -> F128 {
        let mut ret = F128::zero();
        let p_twists : Vec<_> = self.p_evs.iter().enumerate().map(|(i, x)|x.frob(i as i32)).collect();
        let q_twists : Vec<_> = self.q_evs.iter().enumerate().map(|(i, x)|x.frob(i as i32)).collect();
        for i in 0..128 {
            ret += F128::basis(i) * pi(i, &p_twists) * pi(i, &q_twists);
        }
        ret
    } 
}


impl AndcheckProver {
    pub fn new(pt: Vec<F128>, p: Vec<F128>, q: Vec<F128>, evaluation_claim: F128, phase_switch: usize, check_correct: bool) -> Self {
        assert!(1 << pt.len() == p.len());
        assert!(1 << pt.len() == q.len());
        assert!(phase_switch < pt.len());
        if check_correct {
            assert!(
                p.iter().zip_eq(q.iter()).zip_eq(eq_poly(&pt).iter()).fold(F128::zero(), |acc, ((&p, &q), &e)| {acc + (p & q) * e})
                ==
                evaluation_claim
            )
        }

        // Represent values in (0, 1, \infty)^{c+1} (0, 1)^{n-c-1}
        // Replace options with maybeuninit when debugged.
        
        let p_ext = extend_table(&p, pt.len(), phase_switch);
        let q_ext = extend_table(&q, pt.len(), phase_switch);
        let p_q_ext = p_ext.iter().zip_eq(q_ext.iter()).map(|(a, b)| *a & *b).collect();

        Self{pt, p, q, evaluation_claim, c: phase_switch, challenges: vec![], p_q_ext: Some(p_q_ext)}
    }

    pub fn num_vars(&self) -> usize {
        self.pt.len()
    }

    pub fn curr_round(&self) -> usize {
        self.challenges.len()
    }

    pub fn round(&mut self, round_challenge: F128) -> RoundResponse {
        let round = self.curr_round();
        let num_vars = self.num_vars();
        let c = self.c;
        assert!(round < num_vars, "Protocol has already finished.");
        let curr_phase_1 = round <= c;

        let pt = &self.pt;

        let pt_l = &pt[..round];
        let pt_g = &pt[(round + 1)..];
        let pt_r = pt[round];

        if curr_phase_1 {
            // PHASE 1:
            let p_q_ext = self.p_q_ext.as_mut().unwrap();

            let eq_evs = eq_poly(&pt_g); // eq(x, pt_{>})
            let mut poly_deg_2 = vec![F128::zero(); 3]; //Evaluations in 0, 1 and \infty
            let phase1_dims = c - round;
            let pow3 = 3usize.pow(phase1_dims as u32);
            
            for i in 0..(1 << num_vars - c - 1) {
                for j in 0..(1 << phase1_dims) {
                    let index = (i << phase1_dims) + j;
                    let trindex = i * pow3 + bits_to_trits(j);
                    let multiplier = eq_evs[index];
                    poly_deg_2.iter_mut()
                        .zip(p_q_ext[3*trindex .. 3*trindex + 3].iter())
                        .map(|(a, b)| *a += *b * multiplier).count();
                }
            }

            // Cast poly to coefficient form
            // For f(x) = a + bx + cx^2
            // f(0) = a
            // f(\infty) = c
            // f(1) = a+b+c
            // => b = f(1) + f(0) + f(\infty)

            let tmp = poly_deg_2[0];
            poly_deg_2[1] += tmp;
            let tmp = poly_deg_2[2];
            poly_deg_2[1] += tmp;

            let eq_y_multiplier = eq_ev(&self.challenges, &pt_l);

            poly_deg_2.iter_mut().map(|c| *c *= eq_y_multiplier).count();

            // eq(t, pt_r) = t pt_r + (1 - t) (1 - pt_r) = (1+pt_r) + t
            let eq_t = vec![pt_r + F128::one(), F128::one()];

            let poly_final = vec![
                eq_t[0] * poly_deg_2[0],
                eq_t[0] * poly_deg_2[1] + eq_t[1] * poly_deg_2[0],
                eq_t[0] * poly_deg_2[2] + eq_t[1] * poly_deg_2[1],
                eq_t[1] * poly_deg_2[2],
            ];

            let r2 = round_challenge * round_challenge;
            let r3 = round_challenge * r2;

            assert!(poly_final[1] + poly_final[2] + poly_final[3] == self.evaluation_claim);

            self.evaluation_claim = poly_final[0] + poly_final[1] * round_challenge + poly_final[2] * r2 + poly_final[3] * r3;
            self.challenges.push(round_challenge);
            self.p_q_ext = Some((0..(p_q_ext.len()/3)).map(|i| {
                let chunk = &p_q_ext[3 * i .. 3 * i + 3];
                chunk[0] + (chunk[0] + chunk[1] + chunk[2]) * round_challenge + chunk[2] * r2
            }).collect());

            return RoundResponse{values: poly_final}
        } else {
            todo!()
        };
    }


    pub fn finish(&self) -> FinalClaim {
        assert!(self.curr_round() == self.num_vars(), "Protocol is not finished.");
        let mut inverse_orbit = vec![];
        let mut pt = self.challenges.clone();
        for _ in 0..128 {
            pt.iter_mut().map(|x| *x *= *x).count();
            inverse_orbit.push(pt.clone());
        }
        inverse_orbit.reverse();

        let mut p_evs = vec![];
        let mut q_evs = vec![];

        for i in 0..128 {
            p_evs.push(evaluate(&self.p, &inverse_orbit[i]));
            q_evs.push(evaluate(&self.q, &inverse_orbit[i]));
        }

        FinalClaim { p_evs, q_evs }
    }
}



#[cfg(test)]
mod tests {
    use std::iter::{repeat_with};

    use itertools::Itertools;
    use num_traits::Zero;
    use rand::rngs::OsRng;

    use crate::{andcheck::eq_ev, field::F128};

    use super::{eq_poly, evaluate, extend_table, AndcheckProver};

    #[test]
    fn test_eq_ev() {
        let rng = &mut OsRng;
        let num_vars = 5;

        let x : Vec<_> = repeat_with(|| F128::rand(rng)).take(num_vars).collect();
        let y : Vec<_> = repeat_with(|| F128::rand(rng)).take(num_vars).collect();
        

        assert!(eq_ev(&x, &y) == evaluate(&eq_poly(&x), &y));
    }

    #[test]
    /// WARNING: THIS TEST WILL DO NOTHING AFTER WE SWITCH TO MAYBEUNINITS.
    /// NOW IT CHECKS INTEGRITY
    fn extend_table_collisions() {
        let rng = &mut OsRng;
        for i in 0..7 {
            let table = repeat_with(|| F128::rand(rng)).take(1 << i).collect_vec();
            for c in 0..i {
                let ret = extend_table(&table, i, c);
                assert!(ret.len() == 3usize.pow((c+1) as u32)*2usize.pow((i-c-1) as u32));
            }
        }
    }

    #[test]
    fn twists_work_as_expected() {
        let rng = &mut OsRng;
        let num_vars = 5;
        let pt : Vec<_> = repeat_with(|| F128::rand(rng)).take(num_vars).collect();
        let p : Vec<_> = repeat_with(|| F128::rand(rng)).take(1 << num_vars).collect();

        for i in 0..128 {
            let inv_twisted_pt = pt.iter().map(|x| x.frob(- (i as i32))).collect_vec();
            let ev = evaluate(&p, &inv_twisted_pt);
            let twisted_p = p.iter().map(|x|x.frob(i as i32)).collect_vec();
            assert!(ev.frob(i as i32) == evaluate(&twisted_p, &pt));
        }
    }

    #[test]
    fn verify_prover() {
        let rng = &mut OsRng;
        let num_vars = 3;

        let pt : Vec<_> = repeat_with(|| F128::rand(rng)).take(num_vars).collect();
        let p : Vec<_> = repeat_with(|| F128::rand(rng)).take(1 << num_vars).collect();
        let q : Vec<_> = repeat_with(|| F128::rand(rng)).take(1 << num_vars).collect();

        let p_zip_q : Vec<_> = p.iter().zip_eq(q.iter()).map(|(x, y)| *x & *y).collect();
        //let evaluation_claim = evaluate(&p_zip_q, &pt);
        let evaluation_claim = p_zip_q.iter().zip(eq_poly(&pt).iter()).fold(F128::zero(), |acc, (x, y)|acc + *x * *y);

        let mut prover = AndcheckProver::new(pt, p, q, evaluation_claim, 2,true);

        for i in 0..num_vars {
            let round_challenge = F128::rand(rng);
            prover.round(round_challenge);
        }

        assert!(
            prover.finish().apply_algebraic_combinator() * eq_ev(&prover.pt, &prover.challenges)
            ==
            prover.evaluation_claim
        )
    }
}