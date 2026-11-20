//! The signal processing behind time-difference captures: complex samples, an FFT,
//! channel filters, an FM discriminator and GCC-PHAT cross-correlation.

use std::f64::consts::{PI, TAU};
use std::ops::{Add, Mul, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct C {
    pub re: f64,
    pub im: f64,
}

impl C {
    pub fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }

    pub fn conj(self) -> Self {
        Self::new(self.re, -self.im)
    }

    pub fn norm_sqr(self) -> f64 {
        self.re * self.re + self.im * self.im
    }

    pub fn scale(self, k: f64) -> Self {
        Self::new(self.re * k, self.im * k)
    }

    pub fn arg(self) -> f64 {
        self.im.atan2(self.re)
    }

    pub fn cis(phase: f64) -> Self {
        let (s, c) = phase.sin_cos();
        Self::new(c, s)
    }
}

impl Add for C {
    type Output = C;
    fn add(self, o: C) -> C {
        C::new(self.re + o.re, self.im + o.im)
    }
}

impl Sub for C {
    type Output = C;
    fn sub(self, o: C) -> C {
        C::new(self.re - o.re, self.im - o.im)
    }
}

impl Mul for C {
    type Output = C;
    fn mul(self, o: C) -> C {
        C::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }
}

/// In-place radix-2 FFT; `buf.len()` must be a power of two. The inverse is scaled by 1/n.
pub fn fft(buf: &mut [C], inverse: bool) {
    let n = buf.len();
    assert!(n.is_power_of_two(), "fft length {n} is not a power of two");
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            buf.swap(i, j);
        }
    }
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut len = 2;
    while len <= n {
        let w = C::cis(sign * TAU / len as f64);
        for start in (0..n).step_by(len) {
            let mut wk = C::new(1.0, 0.0);
            for k in 0..len / 2 {
                let u = buf[start + k];
                let v = buf[start + k + len / 2] * wk;
                buf[start + k] = u + v;
                buf[start + k + len / 2] = u - v;
                wk = wk * w;
            }
        }
        len <<= 1;
    }
    if inverse {
        let k = 1.0 / n as f64;
        buf.iter_mut().for_each(|x| *x = x.scale(k));
    }
}

/// Hamming-windowed sinc low-pass with `cutoff` as a fraction of the sample rate, unity gain.
pub fn lowpass(cutoff: f64, taps: usize) -> Vec<f64> {
    let m = (taps - 1) as f64;
    let mut h: Vec<f64> = (0..taps)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (TAU * cutoff * x).sin() / (PI * x)
            };
            sinc * (0.54 - 0.46 * (TAU * i as f64 / m).cos())
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|x| *x /= sum);
    h
}

/// One unsigned 8-bit I/Q pair as rtl_sdr writes it, scaled to about +-1.
pub fn from_u8(i: u8, q: u8) -> C {
    C::new((i as f64 - 127.5) / 127.5, (q as f64 - 127.5) / 127.5)
}

/// Share of raw bytes at the ends of the ADC's range: clipping from an overloaded front end.
pub fn clipped(raw: &[u8]) -> f64 {
    if raw.is_empty() {
        return 0.0;
    }
    raw.iter().filter(|b| **b == 0 || **b == 255).count() as f64 / raw.len() as f64
}

/// Mix raw 8-bit I/Q at `fs` down by `offset_hz`, filter with `h` and keep every
/// `decim`th sample. Only the kept outputs are computed.
pub fn channelize(raw: &[u8], fs: f64, offset_hz: f64, h: &[f64], decim: usize) -> Vec<C> {
    let n = raw.len() / 2;
    let step = C::cis(-TAU * offset_hz / fs);
    let mut rot = C::new(1.0, 0.0);
    let mut mixed: Vec<(f32, f32)> = Vec::with_capacity(n);
    for (k, [i, q]) in raw.as_chunks::<2>().0.iter().enumerate() {
        let y = from_u8(*i, *q) * rot;
        mixed.push((y.re as f32, y.im as f32));
        rot = rot * step;
        // keep the rotator on the unit circle
        if k % 1024 == 1023 {
            rot = rot.scale(1.0 / rot.norm_sqr().sqrt());
        }
    }
    let mut out = Vec::with_capacity(n / decim + 1);
    let mut i = h.len() - 1;
    while i < n {
        let mut acc = C::default();
        for (m, &w) in h.iter().enumerate() {
            let (re, im) = mixed[i - m];
            acc.re += w * re as f64;
            acc.im += w * im as f64;
        }
        out.push(acc);
        i += decim;
    }
    out
}

/// Instantaneous frequency in cycles per sample, mean removed: a carrier offset between
/// receivers becomes a constant and drops out.
pub fn discriminator(x: &[C]) -> Vec<f64> {
    let mut d: Vec<f64> = x
        .windows(2)
        .map(|w| (w[1] * w[0].conj()).arg() / TAU)
        .collect();
    let mean = d.iter().sum::<f64>() / d.len().max(1) as f64;
    d.iter_mut().for_each(|v| *v -= mean);
    d
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Peak {
    /// samples by which the second signal lags the first, with sub-sample interpolation
    pub lag: f64,
    /// peak height over the spread of the rest of the search window
    pub psr: f64,
}

/// GCC-PHAT between `a[s..e]` and `b`, searching lags within `w` of `center`: the lag k
/// at which b[n] matches a[n - k]. Only frequencies up to `band` (a fraction of the sample
/// rate, at most 0.5) count, so bins holding nothing but noise don't jitter the peak.
pub fn gcc_window(
    a: &[f64],
    b: &[f64],
    s: usize,
    e: usize,
    center: f64,
    w: usize,
    band: f64,
) -> Option<Peak> {
    let e = e.min(a.len());
    if s >= e {
        return None;
    }
    let seg_a = &a[s..e];
    let c = center.round() as i64;
    let o = (s as i64 + c - w as i64).clamp(0, b.len() as i64) as usize;
    let end = ((e as i64 + c + w as i64).clamp(0, b.len() as i64) as usize).max(o);
    let seg_b = &b[o..end];
    if seg_b.len() < 16 || seg_a.len() < 16 {
        return None;
    }
    let n = (seg_a.len() + seg_b.len()).next_power_of_two();
    let mut fa: Vec<C> = seg_a.iter().map(|v| C::new(*v, 0.0)).collect();
    fa.resize(n, C::default());
    let mut fb: Vec<C> = seg_b.iter().map(|v| C::new(*v, 0.0)).collect();
    fb.resize(n, C::default());
    fft(&mut fa, false);
    fft(&mut fb, false);
    let top = (band * n as f64) as usize;
    let mut g: Vec<C> = fa
        .iter()
        .zip(&fb)
        .enumerate()
        .map(|(k, (x, y))| {
            if k.min(n - k) > top {
                return C::default();
            }
            let p = x.conj() * *y;
            p.scale(1.0 / (p.norm_sqr().sqrt() + 1e-12))
        })
        .collect();
    let spec = g.clone();
    fft(&mut g, true);
    // lag in the segments' terms, then back to the whole signals'
    let shift = o as i64 - s as i64;
    let at = |k: i64| -> f64 {
        let idx = (k - shift).rem_euclid(n as i64) as usize;
        g[idx].re
    };
    let (lo, hi) = (c - w as i64, c + w as i64);
    let (mut best, mut bv) = (lo, f64::MIN);
    for k in lo..=hi {
        let v = at(k);
        if v > bv {
            bv = v;
            best = k;
        }
    }
    let rest: Vec<f64> = (lo..=hi).filter(|k| (k - best).abs() > 3).map(at).collect();
    let mean = rest.iter().sum::<f64>() / rest.len().max(1) as f64;
    let sd = (rest.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / rest.len().max(1) as f64)
        .sqrt()
        .max(1e-12);
    // the whitened peak is about a sample wide, too narrow for a parabola: find its top
    // by evaluating the correlation between samples instead
    let (mut a, mut z) = ((best - shift) as f64 - 1.0, (best - shift) as f64 + 1.0);
    for _ in 0..40 {
        let (m1, m2) = (a + (z - a) / 3.0, z - (z - a) / 3.0);
        if corr_at(&spec, m1) < corr_at(&spec, m2) {
            a = m1;
        } else {
            z = m2;
        }
    }
    Some(Peak {
        lag: shift as f64 + (a + z) / 2.0,
        psr: (bv - mean) / sd,
    })
}

/// The inverse transform of `spec` at a fractional lag `tau`.
fn corr_at(spec: &[C], tau: f64) -> f64 {
    let n = spec.len();
    let w = C::cis(TAU * tau / n as f64);
    let mut wk = C::new(1.0, 0.0);
    let mut acc = spec[0];
    for k in 1..n / 2 {
        wk = wk * w;
        acc = acc + spec[k] * wk + spec[n - k] * wk.conj();
    }
    acc = acc + spec[n / 2] * C::cis(PI * tau);
    acc.re / n as f64
}

/// Averaged power spectrum of raw 8-bit I/Q, lowest frequency first, in `nfft` bins.
pub fn psd(raw: &[u8], nfft: usize) -> Vec<f64> {
    let mut acc = vec![0.0; nfft];
    let mut count = 0;
    let win: Vec<f64> = (0..nfft)
        .map(|i| 0.5 - 0.5 * (TAU * i as f64 / nfft as f64).cos())
        .collect();
    let wpow: f64 = win.iter().map(|w| w * w).sum();
    for block in raw.chunks_exact(2 * nfft) {
        let mut buf: Vec<C> = block
            .as_chunks::<2>()
            .0
            .iter()
            .zip(&win)
            .map(|([i, q], w)| from_u8(*i, *q).scale(*w))
            .collect();
        fft(&mut buf, false);
        for (k, x) in buf.iter().enumerate() {
            acc[(k + nfft / 2) % nfft] += x.norm_sqr() / wpow;
        }
        count += 1;
    }
    acc.iter_mut().for_each(|x| *x /= count.max(1) as f64);
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_a_tone_and_inverts() {
        let n = 64;
        let x: Vec<C> = (0..n)
            .map(|i| C::cis(TAU * 5.0 * i as f64 / n as f64))
            .collect();
        let mut f = x.clone();
        fft(&mut f, false);
        let peak = (0..n)
            .max_by(|a, b| f[*a].norm_sqr().total_cmp(&f[*b].norm_sqr()))
            .unwrap();
        assert_eq!(peak, 5);
        fft(&mut f, true);
        assert!(f.iter().zip(&x).all(|(a, b)| (*a - *b).norm_sqr() < 1e-20));
    }

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5
            })
            .collect()
    }

    #[test]
    fn gcc_phat_finds_the_delay() {
        let a = noise(4000, 1);
        // b is a delayed by 37 samples, plus noise
        let extra = noise(4000, 2);
        let b: Vec<f64> = (0..4000)
            .map(|n| if n >= 37 { a[n - 37] } else { 0.0 } + 0.3 * extra[n])
            .collect();
        let p = gcc_window(&a, &b, 0, 4000, 30.0, 100, 0.5).unwrap();
        assert!((p.lag - 37.0).abs() < 0.3, "{p:?}");
        assert!(p.psr > 20.0, "{p:?}");
        // a sub-window gives the same lag
        let w = gcc_window(&a, &b, 1000, 2000, 37.0, 50, 0.5).unwrap();
        assert!((w.lag - 37.0).abs() < 0.3, "{w:?}");
        // unrelated signals have no clear peak
        let q = gcc_window(&a, &noise(4000, 3), 0, 4000, 0.0, 300, 0.5).unwrap();
        assert!(q.psr < 8.0, "{q:?}");
    }

    #[test]
    fn channelize_and_discriminate_an_offset_tone() {
        let fs = 256_000.0;
        let off = 40_000.0;
        // a tone at +41 kHz: after mixing down 40 kHz it reads as +1 kHz
        let raw: Vec<u8> = (0..20_000)
            .flat_map(|n| {
                let x = C::cis(TAU * 41_000.0 * n as f64 / fs).scale(100.0);
                [(x.re + 127.5) as u8, (x.im + 127.5) as u8]
            })
            .collect();
        let h = lowpass(8_000.0 / fs, 101);
        let y = channelize(&raw, fs, off, &h, 8);
        let f: Vec<f64> = y
            .windows(2)
            .map(|w| (w[1] * w[0].conj()).arg() / TAU * fs / 8.0)
            .collect();
        let mean = f.iter().sum::<f64>() / f.len() as f64;
        assert!((mean - 1000.0).abs() < 20.0, "{mean}");
        // and one 60 kHz away is filtered out
        let gone = channelize(&raw, fs, -20_000.0, &h, 8);
        let p: f64 = gone.iter().map(|c| c.norm_sqr()).sum::<f64>() / gone.len() as f64;
        assert!(p < 1e-3, "{p}");
        assert_eq!(clipped(&[0, 10, 255, 128]), 0.5);
    }
}
