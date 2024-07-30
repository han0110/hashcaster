use std::sync::atomic::{AtomicU64, Ordering};
use bytemuck::cast;

use num_traits::{One, Zero};
use rayon::{iter::{IntoParallelIterator, ParallelIterator}, slice::{ParallelSlice, ParallelSliceMut}};

use crate::{field::F128, protocols::utils::{compute_trit_mappings, eq_ev, eq_poly_sequence, extend_n_tables, restrict, restrict_legacy}, ptr_utils::ConstPtr, traits::{CompressedPoly, SumcheckObject}};

/// A check for any quadratic formula depending on coordinates of polynomials.
/// Good example is any quadratic boolean expression.
/// f: F computes this formula.
/// f_alg : FA is an algebraic formula with F2^128 elements substituted in places that previously were bits.
/// In theory, this function should take 128*N arguments, but I'm not sure how good compiler optimization
/// will be here, and so decided to avoid copies altogether.
/// f_alg : FA takes 3 arguments - a ref to data &[...], a starting index i, and a spacing offset l.
/// It returns 3 applications of algebraic form of f, first with arguments read from data[2*i], data[2*i+l], ...,
/// second with arguments read from data[2*i+1], data[2*i+1+l], ... and third with arguments combined from two
/// previous ones: (data[2*i]+data[2*i+1]), (data[2*i+l] + data[2*i+1+l])...
pub struct BoolCheck<
    const N: usize,
    const M: usize,
    F: Fn([F128; N]) -> [F128; M] + Send + Sync,
    FA: Fn(&[F128], usize, usize) -> [[F128; M]; 3] + Send + Sync,
> {
    f: F,
    f_alg: FA,
    pt: Vec<F128>,
    polys: [Vec<F128>; N], // Input polynomials.
    c: usize, // PHASE SWITCH, round < c => PHASE 1.
    evaluation_claims: [F128; M],
}

impl<
    const N: usize,
    const M: usize,
    F: Fn([F128; N]) -> [F128; M] + Send + Sync,
    FA: Fn(&[F128], usize, usize) -> [[F128; M]; 3] + Send + Sync,
> BoolCheck<N, M, F, FA> {

    pub fn new(f: F, f_alg: FA, polys: [Vec<F128>; N], c: usize, evaluation_claims: [F128; M], pt: Vec<F128>) -> Self {
        Self{f, f_alg, pt, polys, c, evaluation_claims}
    }

    /// Folding round. This is an initial message of the verifier.
    pub fn folding_challenge(self, gamma: F128)
     -> BoolCheckSingle<
        N,
        impl Fn([F128; N]) -> F128 + Send + Sync,
        impl Fn(&[F128], usize, usize) -> [F128; 3] + Send + Sync,
    > {

        let Self { f, f_alg, pt, polys, c, evaluation_claims } = self;
        let f_folded = move |x : [F128; N]| {
            let fx = f(x);
            let mut ret = fx[M-1];
            for i in 0..M-1 {
                ret *= gamma;
                ret += fx[M-2-i];
            }
            ret
        };
        let f_alg_folded = move |x: &[F128], y, z| {
            let fx = f_alg(x, y, z);
            let mut ret = [fx[0][M-1], fx[1][M-1], fx[2][M-1]];
            for i in 0..M-1 {
                ret[0] *= gamma;
                ret[1] *= gamma;
                ret[2] *= gamma;
                ret[0] += fx[0][M-2-i];
                ret[1] += fx[1][M-2-i];
                ret[2] += fx[2][M-2-i];
            }
            ret
        };

        let mut evaluation_claim = evaluation_claims[M-1];
        for i in 0..M-1 {
            evaluation_claim *= gamma;
            evaluation_claim += evaluation_claims[M-2-i];
        }

        BoolCheckSingle::new(
            f_folded,
            f_alg_folded,
            pt,
            polys,
            c,
            evaluation_claim,
        )

    }

}

pub struct BoolCheckSingle<
    const N: usize,
    F: Fn([F128; N]) -> F128 + Send + Sync,
    FA: Fn(&[F128], usize, usize) -> [F128; 3] + Send + Sync,
> {
    f: F,
    f_alg: FA,
    pt: Vec<F128>,

    polys: [Vec<F128>; N], // Input polynomials.
    ext: Option<Vec<F128>>, // Extension of output on 3^{c+1} * 2^{n-c-1}, during first phase.
    polys_coords: Vec<Vec<F128>>, // Coordinates of input polynomials, in the second phase.
                                    // They live in a single contigious array to access using FA.
                                    // Real poly_coords is always the last one, others are preserved
                                    // for caching purposes - they are used in later arguments.
    c: usize, // PHASE SWITCH, round < c => PHASE 1.
    claim: F128,
    challenges: Vec<F128>,
    bits_to_trits_map: Vec<u16>,
    eq_sequence: Vec<Vec<F128>>, // Precomputed eqs of all slices pt[i..].

    round_polys: Vec<CompressedPoly>,
}

impl<
    const N: usize,
    F: Fn([F128; N]) -> F128 + Send + Sync,
    FA: Fn(&[F128], usize, usize) -> [F128; 3] + Send + Sync,
> BoolCheckSingle<N, F, FA> {
    pub fn new(f: F, f_alg: FA, pt: Vec<F128>, polys: [Vec<F128>; N], c: usize, evaluation_claim: F128) -> Self {
        for poly in polys.iter() {
            assert!(poly.len() == 1 << pt.len());
        }
        assert!(c < pt.len());

        let (bit_mapping, trit_mapping) = compute_trit_mappings(c);

        // A bit of ugly signature juggling to satisfy extend.
        let ext = {
            let polys : Vec<&[F128]>= polys.iter().map(|v|v.as_slice()).collect();
            extend_n_tables(&polys, c, &trit_mapping, &f)
        };

        let eq_sequence = eq_poly_sequence(&pt[1..]);
    
        Self {
            f,
            f_alg,
            pt,
            polys,
            ext : Some(ext),
            polys_coords : vec![],
            c,
            claim: evaluation_claim,
            challenges : vec![],
            bits_to_trits_map : bit_mapping,
            eq_sequence,
            round_polys: vec![]
        }
    }

    pub fn curr_round(&self) -> usize {
        self.challenges.len()
    }

    pub fn num_vars(&self) -> usize {
        self.pt.len()
    }
}

impl<
    const N: usize,
    F: Fn([F128; N]) -> F128 + Send + Sync,
    FA: Fn(&[F128], usize, usize) -> [F128; 3] + Send + Sync,
> SumcheckObject for BoolCheckSingle<N, F, FA> {

    fn is_reverse_order(&self) -> bool {
        false
    }

    fn bind(&mut self, t: F128) {
        let round = self.curr_round();
        let num_vars = self.num_vars();
        let c = self.c;
        assert!(round < num_vars, "Protocol has already finished.");
        let curr_phase_1 = round <= c;

        let rpoly = self.round_msg().coeffs(self.claim);
        let t2 = t * t;
        let t3 = t * t * t;
        self.claim = rpoly[0] + t * rpoly[1] + t2 * rpoly[2] + t3 * rpoly[3];
        self.challenges.push(t);

        if curr_phase_1 {
            let ext = self.ext.as_mut().unwrap();

            #[cfg(not(feature = "parallel"))]
            let ext_chunks = ext.chunks(3);

            #[cfg(feature = "parallel")]
            let ext_chunks = ext.par_chunks(3);

            self.ext = Some(
                ext_chunks.map(|chunk| {
                    chunk[0] + (chunk[0] + chunk[1] + chunk[2]) * t + chunk[2] * t2
                }).collect()
            );
        } else {
            let poly_coords = self.polys_coords.last().unwrap();
            let half = 1 << (num_vars - round - 1);

            #[cfg(not(feature = "parallel"))]
            let iter = (0 .. half * 128 * N);
            #[cfg(feature = "parallel")]
            let iter = (0 .. half * 128 * N).into_par_iter();

            let restriction = 
                iter.map(|j| {
                    poly_coords[2 * j] + (poly_coords[2 * j + 1] + poly_coords[2 * j]) * t
                }).collect();

            self.polys_coords.push(restriction);
        }

        if self.curr_round() == c + 1 { // Note that we are in the next round now.
            let _ = self.ext.take(); // it is useless now
            self.polys_coords.push(
                restrict(
                &(self.polys.iter().map(|x|x.as_slice()).collect::<Vec<_>>()),
                &self.challenges,
                num_vars
                )
            );
        }

    }

    fn round_msg(&mut self) -> CompressedPoly {
        let round = self.curr_round();
        let num_vars = self.num_vars();
        let c = self.c;
        assert!(round < num_vars, "Protocol has already finished.");
        
// If already computed, just return cached value and do nothing.
        if self.round_polys.len() > round {
            return self.round_polys.last().unwrap().clone()
        }
        
        let curr_phase_1 = round <= c;

        let pt = &self.pt;

        let pt_l = &pt[..round];
//        let pt_g = &pt[(round + 1)..];
        let pt_r = pt[round];


        if curr_phase_1 {
            // PHASE 1:
            let ext = self.ext.as_ref().unwrap();

            let eq_evs = &self.eq_sequence[num_vars - round - 1]; // eq(x, pt_{>})
            let phase1_dims = c - round;
            let pow3 = 3usize.pow(phase1_dims as u32);

            #[cfg(not(feature = "parallel"))]
            let mut poly_deg_2 =
            (0 .. (1 << num_vars - c - 1)).into_iter().map(|i| {
                let mut pd2_part = [F128::zero(), F128::zero(), F128::zero()];
                for j in 0..(1 << phase1_dims) {
                    let index = (i << phase1_dims) + j;
                    let offset = 3 * (i * pow3 + self.bits_to_trits_map[j] as usize);
                    let multiplier = eq_evs[index];
                    pd2_part[0] += ext[offset] * multiplier;
                    pd2_part[1] += ext[offset + 1] * multiplier;
                    pd2_part[2] += ext[offset + 2] * multiplier;
                }
                pd2_part
            }).fold([F128::zero(), F128::zero(), F128::zero()], |a, b|{
                [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
            });
   
            #[cfg(feature = "parallel")]
            let mut poly_deg_2 =
            (0 .. (1 << num_vars - c - 1)).into_par_iter().map(|i| {
                let mut pd2_part = [F128::zero(), F128::zero(), F128::zero()];
                for j in 0..(1 << phase1_dims) {
                    let index = (i << phase1_dims) + j;
                    let offset = 3 * (i * pow3 + self.bits_to_trits_map[j] as usize);
                    let multiplier = eq_evs[index];
                    pd2_part[0] += ext[offset] * multiplier;
                    pd2_part[1] += ext[offset + 1] * multiplier;
                    pd2_part[2] += ext[offset + 2] * multiplier;
                }
                pd2_part
            }).reduce(||{[F128::zero(), F128::zero(), F128::zero()]}, |a, b|{
                [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
            });


            // Cast poly to coefficient form

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

            let (ret, expected_claim) = CompressedPoly::compress(&poly_final);
            assert!(expected_claim == self.claim); //sanity check - will eventually be optimized out

            assert!(self.round_polys.len() == round, "Impossible.");
            self.round_polys.push(ret.clone());
            ret
        } else {
            let eq_evs = &self.eq_sequence[pt.len() - round - 1];
            let half = eq_evs.len();
            assert!(half == 1 << (num_vars - round - 1));

            let full = half * 2;

            let polys_coords = self.polys_coords.last().unwrap();

            let f_alg = &self.f_alg;

            let poly_deg_2 : [AtomicU64; 6] = [0.into(), 0.into(), 0.into(), 0.into(), 0.into(), 0.into()];

            #[cfg(not(feature = "parallel"))]
            let iter = (0..half).into_iter();

            #[cfg(feature = "parallel")]
            let iter = (0..half).into_par_iter();

            iter.map(|i| {
                let res = cast::<[F128; 3], [u64; 6]> (f_alg(&polys_coords, i, full).map(|x| x * eq_evs[i]));
                poly_deg_2[0].fetch_xor(res[0], Ordering::Relaxed);
                poly_deg_2[1].fetch_xor(res[1], Ordering::Relaxed);
                poly_deg_2[2].fetch_xor(res[2], Ordering::Relaxed);
                poly_deg_2[3].fetch_xor(res[3], Ordering::Relaxed);
                poly_deg_2[4].fetch_xor(res[4], Ordering::Relaxed);
                poly_deg_2[5].fetch_xor(res[5], Ordering::Relaxed);
            }).count();

            let poly_deg_2 : [u64; 6] = poly_deg_2.iter().map(|x| x.load(Ordering::Relaxed)).collect::<Vec<_>>().try_into().unwrap();
            let mut poly_deg_2 = cast::<[u64; 6], [F128; 3]>(poly_deg_2);

            let eq_y_multiplier = eq_ev(&self.challenges, &pt_l);
            poly_deg_2.iter_mut().map(|c| *c *= eq_y_multiplier).count();

            // Cast poly to coefficient form

            let tmp = poly_deg_2[0];
            poly_deg_2[1] += tmp;
            let tmp = poly_deg_2[2];
            poly_deg_2[1] += tmp;

            let eq_t = vec![pt_r + F128::one(), F128::one()];

            let poly_final = vec![
                eq_t[0] * poly_deg_2[0],
                eq_t[0] * poly_deg_2[1] + eq_t[1] * poly_deg_2[0],
                eq_t[0] * poly_deg_2[2] + eq_t[1] * poly_deg_2[1],
                eq_t[1] * poly_deg_2[2],
            ];

            let (ret, expected_claim) = CompressedPoly::compress(&poly_final);
            assert!(expected_claim == self.claim); //sanity check - will eventually be optimized out

            assert!(self.round_polys.len() == round, "Impossible.");
            self.round_polys.push(ret.clone());
            ret
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{iter::repeat_with, time::Instant};
    use rand::rngs::OsRng;
    use crate::protocols::{andcheck::AndcheckProver, utils::eq_poly};
    use super::*;

    fn and_compressed(arg : [F128; 2]) -> [F128; 1] {
        [arg[0] & arg[1]]
    }

    fn and_algebraic(data: &[F128], mut idx_a: usize, offset: usize) -> [[F128; 1]; 3] {
        idx_a *= 2;
        let mut idx_b = idx_a + offset * 128;

        let mut ret = [
            [F128::basis(0) * data[idx_a] * data[idx_b]],
            [F128::basis(0) * data[idx_a + 1] * data[idx_b + 1]],
            [F128::basis(0) * (data[idx_a] + data[idx_a + 1]) * (data[idx_b] + data[idx_b + 1])],
        ];

        for i in 1..128 {
            idx_a += offset;
            idx_b += offset;

            ret[0][0] += F128::basis(i) * data[idx_a] * data[idx_b];
            ret[1][0] += F128::basis(i) * data[idx_a + 1] * data[idx_b + 1];
            ret[2][0] += F128::basis(i) * (data[idx_a] + data[idx_a + 1]) * (data[idx_b] + data[idx_b + 1]);
        }

        ret
    }


    #[test]
    fn new_andcheck() {
        let rng = &mut OsRng;

        let num_vars = 20;

        let pt : Vec<_> = repeat_with(|| F128::rand(rng)).take(num_vars).collect();
        let p : Vec<_> = repeat_with(|| F128::rand(rng)).take(1 << num_vars).collect();
        let q : Vec<_> = repeat_with(|| F128::rand(rng)).take(1 << num_vars).collect();

        let p_zip_q : Vec<_> = p.iter().zip(q.iter()).map(|(x, y)| *x & *y).collect();
        //let evaluation_claim = evaluate(&p_zip_q, &pt);
        let evaluation_claim = p_zip_q.iter().zip(eq_poly(&pt).iter()).fold(F128::zero(), |acc, (x, y)|acc + *x * *y);

        let phase_switch = 5;

        let mut legacy_instance = AndcheckProver::new(pt.clone(), p.clone(), q.clone(), evaluation_claim, phase_switch, true);


        let instance = BoolCheck::new(
            and_compressed, 
            and_algebraic, 
            [p, q], 
            phase_switch, 
            [evaluation_claim],
            pt
        );

        let mut instance = instance.folding_challenge(F128::rand(rng));

        let mut challenges = vec![];
        for _ in 0..num_vars {
            challenges.push(F128::rand(rng));
        }

        let start = Instant::now();

        for i in 0..num_vars {
            legacy_instance.round(challenges[i]);
        }

        let end = Instant::now();

        println!("Legacy - time elapsed: {} ms", (end-start).as_millis());

        let start = Instant::now();

        for i in 0..num_vars {
            instance.bind(challenges[i]);
        }

        let end = Instant::now();

        println!("New - time elapsed: {} ms", (end-start).as_millis());


        assert!(legacy_instance.evaluation_claim == instance.claim)


    }
}
