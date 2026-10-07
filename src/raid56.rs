//! RAID5 and RAID6: where a data element lives, and how a lost one is
//! rebuilt from the rest of its full stripe (#268).
//!
//! # The layout
//!
//! A parity chunk has `n` stripes, one per device, of which `k` hold data
//! in any one full stripe: `n - 1` for RAID5 and `n - 2` for RAID6. The
//! chunk's logical range is cut into `stripe_len` elements; `k` of them in
//! a row make a full stripe, and full stripe `f` occupies the same
//! `stripe_len` window, `f * stripe_len` into every one of the `n` stripes.
//! Within it, data element `i` is on stripe `(i + f) mod n`, P on
//! `(k + f) mod n` and, for RAID6, Q on `(k + 1 + f) mod n`: parity
//! rotates one stripe per full stripe, so no one device holds all of it.
//!
//! # The parity
//!
//! P is the XOR of the data elements. Q is `sum(g^i * D_i)` over GF(2^8)
//! with the polynomial `x^8 + x^4 + x^3 + x^2 + 1` (0x11d) and generator
//! `g = 2`, the code the Linux RAID6 driver uses and H. Peter Anvin's
//! paper *The mathematics of RAID-6* derives. Both are checked against
//! pools the kernel wrote in `tests/raid56_oracle.rs`, which is the only
//! evidence that counts: the unit tests below generate parity with the
//! same arithmetic that rebuilds from it.
//!
//! # Rebuilding is a read attempt, not a copy
//!
//! A mirrored chunk has copies; a parity chunk has one copy and several
//! ways of computing it again. Each is numbered as a read attempt after
//! the direct read, so the read path's fallback — try the next attempt
//! when a checksum fails — works unchanged:
//!
//! | attempt | reads |
//! |---|---|
//! | 0 | the element itself |
//! | 1 | the other data elements and P |
//! | 2 (RAID6) | the other data elements and Q |
//! | 3.. (RAID6) | P and Q and every data element but this one and one other, both rebuilt together |
//!
//! Attempts 3 onwards pair the wanted element with each other data
//! element in turn, because which second element is also bad is not
//! something a reader can know; a checksum on the result says whether
//! the guess was right.

use crate::error::{Error, Result};

/// `x^8 + x^4 + x^3 + x^2 + 1`, the low byte: the RAID6 field polynomial.
const POLY: u16 = 0x11d;

/// `g^i` for `i` in `0..255`, and `log[g^i] = i`.
struct Tables {
    exp: [u8; 255],
    log: [u8; 256],
}

const fn tables() -> Tables {
    let mut exp = [0u8; 255];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
        i += 1;
    }
    Tables { exp, log }
}

static GF: Tables = tables();

/// `g^e`, for any exponent.
fn pow_g(e: usize) -> u8 {
    GF.exp[e % 255]
}

/// `a * b` in GF(2^8).
fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    GF.exp[(usize::from(GF.log[a as usize]) + usize::from(GF.log[b as usize])) % 255]
}

/// `1 / a` in GF(2^8); `a` is never zero here.
fn inv(a: u8) -> u8 {
    GF.exp[(255 - usize::from(GF.log[a as usize])) % 255]
}

/// Where one element of a full stripe is: the stripe index in the chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Which stripe each data element is on, by data index.
    pub data_stripes: Vec<usize>,
    /// The stripe P is on.
    pub p: usize,
    /// The stripe Q is on, for RAID6.
    pub q: Option<usize>,
}

/// The most stripes a parity chunk is read with. The kernel caps a chunk
/// at far fewer devices than this in practice; a chunk claiming more is
/// refused rather than allocated for.
pub const MAX_STRIPES: usize = 256;

/// Where everything in full stripe `full` of an `n`-stripe chunk with
/// `parity` parity elements lives.
pub fn placement(n: usize, parity: usize, full: u64) -> Result<Placement> {
    if n > MAX_STRIPES || n <= parity {
        return Err(Error::BadChunkItem(format!(
            "a parity chunk with {n} stripes and {parity} parity elements"
        )));
    }
    let k = n - parity;
    let rot = (full % n as u64) as usize;
    Ok(Placement {
        data_stripes: (0..k).map(|i| (i + rot) % n).collect(),
        p: (k + rot) % n,
        q: (parity == 2).then_some((k + 1 + rot) % n),
    })
}

/// How a lost data element is computed again; see the module table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebuild {
    /// From the other data elements and P.
    P,
    /// From the other data elements and Q.
    Q,
    /// From P and Q, with data element `other` lost as well.
    PQ {
        /// The second lost data element.
        other: usize,
    },
}

impl Rebuild {
    /// The rebuild read attempt `attempt` (1 onwards) uses for data
    /// element `want` of `data`, or `None` past the last.
    pub fn for_attempt(attempt: usize, want: usize, data: usize, raid6: bool) -> Option<Self> {
        match attempt {
            1 => Some(Rebuild::P),
            2 if raid6 => Some(Rebuild::Q),
            a if raid6 && a >= 3 => {
                let nth = a - 3;
                (0..data)
                    .filter(|&i| i != want)
                    .nth(nth)
                    .map(|other| Rebuild::PQ { other })
            }
            _ => None,
        }
    }

    /// How many read attempts a parity chunk with `data` data elements
    /// offers, the direct read included.
    pub fn attempts(data: usize, raid6: bool) -> usize {
        if raid6 {
            3 + data.saturating_sub(1)
        } else {
            2
        }
    }
}

/// Data element `want`, computed from the full stripe's other elements.
///
/// `data[i]` is data element `i` as read, `None` for the ones the method
/// does not read (`want`, and `other` for [`Rebuild::PQ`]); `p` and `q`
/// likewise. Every present element is the same length.
pub fn rebuild(
    method: Rebuild,
    want: usize,
    data: &[Option<Vec<u8>>],
    p: Option<&[u8]>,
    q: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let missing = |what: &str| Error::BadChunkItem(format!("a parity rebuild without {what}"));
    match method {
        Rebuild::P => {
            let mut out = p.ok_or_else(|| missing("P"))?.to_vec();
            for (i, d) in data.iter().enumerate() {
                if i == want {
                    continue;
                }
                xor_into(
                    &mut out,
                    d.as_deref().ok_or_else(|| missing("a data element"))?,
                );
            }
            Ok(out)
        }
        Rebuild::Q => {
            // Q ^ sum over the others of g^i * D_i is g^want * D_want.
            let mut acc = q.ok_or_else(|| missing("Q"))?.to_vec();
            for (i, d) in data.iter().enumerate() {
                if i == want {
                    continue;
                }
                mul_xor_into(
                    &mut acc,
                    d.as_deref().ok_or_else(|| missing("a data element"))?,
                    pow_g(i),
                );
            }
            let scale = inv(pow_g(want));
            for b in &mut acc {
                *b = mul(*b, scale);
            }
            Ok(acc)
        }
        Rebuild::PQ { other } => {
            if other == want {
                return Err(missing("a second element to rebuild"));
            }
            // With x = want and y = other lost:
            //   Pxy = P ^ sum D_i           = D_x ^ D_y
            //   Qxy = Q ^ sum g^i D_i       = g^x D_x ^ g^y D_y
            // so (g^x ^ g^y) D_x = g^y Pxy ^ Qxy.
            let mut pxy = p.ok_or_else(|| missing("P"))?.to_vec();
            let mut qxy = q.ok_or_else(|| missing("Q"))?.to_vec();
            for (i, d) in data.iter().enumerate() {
                if i == want || i == other {
                    continue;
                }
                let d = d.as_deref().ok_or_else(|| missing("a data element"))?;
                xor_into(&mut pxy, d);
                mul_xor_into(&mut qxy, d, pow_g(i));
            }
            let (gx, gy) = (pow_g(want), pow_g(other));
            let scale = inv(gx ^ gy);
            Ok(pxy
                .iter()
                .zip(&qxy)
                .map(|(&pb, &qb)| mul(mul(gy, pb) ^ qb, scale))
                .collect())
        }
    }
}

fn xor_into(acc: &mut [u8], d: &[u8]) {
    for (a, b) in acc.iter_mut().zip(d) {
        *a ^= b;
    }
}

fn mul_xor_into(acc: &mut [u8], d: &[u8], factor: u8) {
    for (a, b) in acc.iter_mut().zip(d) {
        *a ^= mul(*b, factor);
    }
}

#[cfg(test)]
mod tests {
    //! Self-consistency only: parity is generated here by the arithmetic
    //! that rebuilds from it. `tests/raid56_oracle.rs` is the check
    //! against the kernel.

    use super::*;

    fn p_of(data: &[Vec<u8>]) -> Vec<u8> {
        let mut p = vec![0; data[0].len()];
        for d in data {
            xor_into(&mut p, d);
        }
        p
    }

    fn q_of(data: &[Vec<u8>]) -> Vec<u8> {
        let mut q = vec![0; data[0].len()];
        for (i, d) in data.iter().enumerate() {
            mul_xor_into(&mut q, d, pow_g(i));
        }
        q
    }

    fn sample(k: usize) -> Vec<Vec<u8>> {
        (0..k)
            .map(|i| {
                (0..64u32)
                    .map(|j| (j * 31 + i as u32 * 97 + 7) as u8)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_field_is_the_raid6_field() {
        // g^8 reduced by 0x11d is 0x1d; every non-zero element has an
        // inverse.
        assert_eq!(pow_g(8), 0x1d);
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1, "{a}");
        }
    }

    #[test]
    fn parity_rotates_one_stripe_per_full_stripe() {
        let p0 = placement(3, 1, 0).unwrap();
        assert_eq!(&p0.data_stripes[..2], &[0, 1]);
        assert_eq!((p0.p, p0.q), (2, None));
        let p1 = placement(3, 1, 1).unwrap();
        assert_eq!(&p1.data_stripes[..2], &[1, 2]);
        assert_eq!(p1.p, 0);
        let q = placement(4, 2, 3).unwrap();
        assert_eq!(&q.data_stripes[..2], &[3, 0]);
        assert_eq!((q.p, q.q), (1, Some(2)));
        assert!(placement(2, 2, 0).is_err());
    }

    #[test]
    fn every_method_rebuilds_every_element() {
        for k in 2..6 {
            let data = sample(k);
            let (p, q) = (p_of(&data), q_of(&data));
            for want in 0..k {
                let mut seen: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
                seen[want] = None;
                for method in [Rebuild::P, Rebuild::Q] {
                    let got = rebuild(method, want, &seen, Some(&p), Some(&q)).unwrap();
                    assert_eq!(got, data[want], "{method:?} k={k} want={want}");
                }
                for other in (0..k).filter(|&o| o != want) {
                    let mut two = seen.clone();
                    two[other] = None;
                    let got =
                        rebuild(Rebuild::PQ { other }, want, &two, Some(&p), Some(&q)).unwrap();
                    assert_eq!(got, data[want], "PQ k={k} want={want} other={other}");
                }
            }
        }
    }

    #[test]
    fn attempts_cover_every_second_element_once() {
        assert_eq!(Rebuild::attempts(2, false), 2);
        assert_eq!(Rebuild::attempts(3, true), 5);
        let methods: Vec<_> = (1..Rebuild::attempts(3, true))
            .map(|a| Rebuild::for_attempt(a, 1, 3, true).unwrap())
            .collect();
        assert_eq!(
            methods,
            vec![
                Rebuild::P,
                Rebuild::Q,
                Rebuild::PQ { other: 0 },
                Rebuild::PQ { other: 2 }
            ]
        );
        assert_eq!(Rebuild::for_attempt(2, 0, 2, false), None);
    }

    #[test]
    fn a_missing_input_is_an_error_not_a_guess() {
        let data = sample(2);
        let seen = vec![None, Some(data[1].clone())];
        assert!(rebuild(Rebuild::P, 0, &seen, None, None).is_err());
        assert!(rebuild(Rebuild::Q, 0, &seen, None, None).is_err());
        assert!(rebuild(Rebuild::PQ { other: 0 }, 0, &seen, None, None).is_err());
    }
}
