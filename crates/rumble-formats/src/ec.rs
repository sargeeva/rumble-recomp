//! ECDSA on sect233r1, the curve the Wii signs saves and certificates with
//! (after segher's tachtig and Dolphin's `ec.cpp`). Keys and signature
//! halves are 30-byte big-endian numbers; a public key is x then y.
//!
//! Field elements are polynomials over GF(2) modulo x^233 + x^74 + 1, kept
//! in four 64-bit limbs; points are affine, `None` being the point at
//! infinity. Nothing here is constant-time: it signs a save export now and
//! then, not secrets an attacker can time.

use num_bigint::BigUint;

type Elt = [u64; 4];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Point {
    x: Elt,
    y: Elt,
}

// The curve y^2 + xy = x^3 + x^2 + b and its base point G of prime order N
// (SEC 2, sect233r1).
const B: &str = "0066647ede6c332c7f8c0923bb58213b333b20e9ce4281fe115f7d8f90ad";
const GX: &str = "00fac9dfcbac8313bb2139f1bb755fef65bc391f8b36f8f8eb7371fd558b";
const GY: &str = "01006a08a41903350678e58528bebf8a0beff867a7ca36716f7e01f81052";
const N: &str = "01000000000000000000000000000013e974e72f8a6922031d2603cfe0d7";

fn hex30(s: &str) -> [u8; 30] {
    let mut b = [0; 30];
    for (k, out) in b.iter_mut().enumerate() {
        *out = u8::from_str_radix(&s[2 * k..2 * k + 2], 16).unwrap();
    }
    b
}

fn elt(b: &[u8]) -> Elt {
    let mut e = [0u64; 4];
    for (k, &byte) in b.iter().rev().enumerate() {
        e[k / 8] |= (byte as u64) << (8 * (k % 8));
    }
    e
}

fn elt_bytes(e: &Elt) -> [u8; 30] {
    let mut b = [0; 30];
    for (k, out) in b.iter_mut().rev().enumerate() {
        *out = (e[k / 8] >> (8 * (k % 8))) as u8;
    }
    b
}

fn add(a: &Elt, b: &Elt) -> Elt {
    [a[0] ^ b[0], a[1] ^ b[1], a[2] ^ b[2], a[3] ^ b[3]]
}

fn mul(a: &Elt, b: &Elt) -> Elt {
    let mut r = [0u64; 8];
    for i in 0..233 {
        if a[i / 64] >> (i % 64) & 1 != 0 {
            let (w, s) = (i / 64, i % 64);
            for k in 0..4 {
                r[k + w] ^= b[k] << s;
                if s != 0 {
                    r[k + w + 1] ^= b[k] >> (64 - s);
                }
            }
        }
    }
    // x^233 = x^74 + 1.
    for i in (233..466).rev() {
        if r[i / 64] >> (i % 64) & 1 != 0 {
            r[i / 64] ^= 1 << (i % 64);
            let j = i - 233;
            r[j / 64] ^= 1 << (j % 64);
            let j = j + 74;
            r[j / 64] ^= 1 << (j % 64);
        }
    }
    [r[0], r[1], r[2], r[3]]
}

/// a^(2^233 - 2), the inverse of a nonzero a.
fn inv(a: &Elt) -> Elt {
    let mut x = *a;
    for _ in 0..231 {
        x = mul(&mul(&x, &x), a);
    }
    mul(&x, &x)
}

const ONE: Elt = [1, 0, 0, 0];

fn double(p: Option<Point>) -> Option<Point> {
    let p = p?;
    if p.x == [0; 4] {
        return None;
    }
    let l = add(&p.x, &mul(&p.y, &inv(&p.x)));
    let x = add(&add(&mul(&l, &l), &l), &ONE);
    let y = add(&mul(&p.x, &p.x), &mul(&add(&l, &ONE), &x));
    Some(Point { x, y })
}

fn point_add(p: Option<Point>, q: Option<Point>) -> Option<Point> {
    let (Some(a), Some(b)) = (p, q) else { return p.or(q) };
    if a.x == b.x {
        // The same point, or each other's negation (x, x + y).
        return if a.y == b.y { double(p) } else { None };
    }
    let l = mul(&add(&a.y, &b.y), &inv(&add(&a.x, &b.x)));
    let x = add(&add(&add(&mul(&l, &l), &l), &add(&a.x, &b.x)), &ONE);
    let y = add(&add(&mul(&l, &add(&a.x, &x)), &x), &a.y);
    Some(Point { x, y })
}

fn point_mul(k: &BigUint, p: Option<Point>) -> Option<Point> {
    let mut r = None;
    for i in (0..k.bits()).rev() {
        r = double(r);
        if k.bit(i) {
            r = point_add(r, p);
        }
    }
    r
}

fn on_curve(p: &Point) -> bool {
    // y^2 + xy == x^3 + x^2 + b
    let x2 = mul(&p.x, &p.x);
    let lhs = add(&mul(&p.y, &p.y), &mul(&p.x, &p.y));
    let rhs = add(&add(&mul(&x2, &p.x), &x2), &elt(&hex30(B)));
    lhs == rhs
}

fn g() -> Option<Point> {
    Some(Point { x: elt(&hex30(GX)), y: elt(&hex30(GY)) })
}

fn n() -> BigUint {
    BigUint::from_bytes_be(&hex30(N))
}

fn bytes30(v: &BigUint) -> [u8; 30] {
    let b = v.to_bytes_be();
    let mut out = [0; 30];
    out[30 - b.len()..].copy_from_slice(&b);
    out
}

/// The public key for a private key.
pub fn priv_to_pub(private: &[u8; 30]) -> [u8; 60] {
    let p = point_mul(&BigUint::from_bytes_be(private), g()).expect("private key is not zero mod N");
    let mut out = [0; 60];
    out[..30].copy_from_slice(&elt_bytes(&p.x));
    out[30..].copy_from_slice(&elt_bytes(&p.y));
    out
}

/// Signs a SHA-1 hash: R then S. `nonce` must be fresh random bytes.
pub fn sign(private: &[u8; 30], hash: &[u8; 20], nonce: &[u8; 30]) -> [u8; 60] {
    let n = n();
    let e = BigUint::from_bytes_be(hash);
    let m = BigUint::from_bytes_be(nonce) % &n;
    let m = if m == BigUint::ZERO { BigUint::from(1u32) } else { m };
    let p = point_mul(&m, g()).unwrap();
    let r = BigUint::from_bytes_be(&elt_bytes(&p.x)) % &n;
    let k = BigUint::from_bytes_be(private) % &n;
    let minv = m.modpow(&(&n - 2u32), &n);
    let s = (minv * ((e + &r * k) % &n)) % &n;
    let mut out = [0; 60];
    out[..30].copy_from_slice(&bytes30(&r));
    out[30..].copy_from_slice(&bytes30(&s));
    out
}

/// Checks a signature of a SHA-1 hash against a public key.
pub fn verify(public: &[u8; 60], hash: &[u8; 20], sig: &[u8; 60]) -> bool {
    let n = n();
    let q = Point { x: elt(&public[..30]), y: elt(&public[30..]) };
    if !on_curve(&q) {
        return false;
    }
    let r = BigUint::from_bytes_be(&sig[..30]);
    let s = BigUint::from_bytes_be(&sig[30..]);
    if r == BigUint::ZERO || s == BigUint::ZERO || r >= n || s >= n {
        return false;
    }
    let e = BigUint::from_bytes_be(hash);
    let w = s.modpow(&(&n - 2u32), &n);
    let u1 = (e * &w) % &n;
    let u2 = (&r * &w) % &n;
    let Some(p) = point_add(point_mul(&u1, g()), point_mul(&u2, Some(q))) else { return false };
    BigUint::from_bytes_be(&elt_bytes(&p.x)) % &n == r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constants describe the curve: G is on it and has order N.
    #[test]
    fn base_point_has_order_n() {
        assert!(on_curve(&g().unwrap()));
        assert_eq!(point_mul(&n(), g()), None);
        assert!(point_mul(&(n() - 1u32), g()).is_some());
    }

    #[test]
    fn sign_then_verify() {
        let mut private = [0; 30];
        private[29] = 1;
        private[5] = 0x42;
        let public = priv_to_pub(&private);
        let hash = [7u8; 20];
        let sig = sign(&private, &hash, &[0x5a; 30]);
        assert!(verify(&public, &hash, &sig));
        let mut other = hash;
        other[0] ^= 1;
        assert!(!verify(&public, &other, &sig));
    }
}
