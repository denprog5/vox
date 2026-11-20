//! Голосовые признаки окна: громкость и её разброс, высота голоса и её размах, яркость
//! и «шумность» звука, ритм «ха-ха-ха» (модуляция громкости 3–8 Гц). Модель звуков
//! (CED) знает, *что* звучит, а эти признаки — *как*: именно они отличают крик от речи,
//! когда компрессор выровнял громкость.

use std::f32::consts::PI;

pub const LEN: usize = 16;

const RATE: f32 = 16_000.0;
const N: usize = 400; // 25 мс
const HOP: usize = 160; // 10 мс
const NFFT: usize = 512;

/// 16 признаков для окна моно 16 кГц.
pub fn features(seg: &[f32]) -> [f32; LEN] {
    let mut out = [0.0f32; LEN];
    if seg.len() < N + HOP {
        return out;
    }
    let win: Vec<f32> = (0..N)
        .map(|k| 0.5 - 0.5 * (2.0 * PI * k as f32 / (N - 1) as f32).cos())
        .collect();
    let nb = NFFT / 2 + 1;
    let freq = |k: usize| k as f32 * RATE / NFFT as f32;

    let mut le = Vec::new();
    let mut cent = Vec::new();
    let mut tilt = Vec::new();
    let mut flat = Vec::new();
    let mut flux = Vec::new();
    let mut f0 = Vec::new();
    let mut conf = Vec::new();
    let mut prev_log: Option<Vec<f32>> = None;
    let mut re = vec![0.0f32; NFFT];
    let mut im = vec![0.0f32; NFFT];
    let mut frame = vec![0.0f32; N];

    let mut i = 0;
    while i + N <= seg.len() {
        for k in 0..N {
            frame[k] = seg[i + k] * win[k];
        }
        let e = frame.iter().map(|x| x * x).sum::<f32>() / N as f32 + 1e-10;
        le.push(10.0 * e.log10());

        re.iter_mut().for_each(|v| *v = 0.0);
        im.iter_mut().for_each(|v| *v = 0.0);
        re[..N].copy_from_slice(&frame);
        fft(&mut re, &mut im);
        let spec: Vec<f32> = (0..nb).map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt() + 1e-9).collect();
        let (mut p_all, mut p_f, mut p_lo, mut p_hi) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for (k, s) in spec.iter().enumerate() {
            let p = s * s;
            let f = freq(k);
            p_all += p;
            p_f += p * f;
            if f > 1500.0 {
                p_hi += p;
            } else if f > 100.0 {
                p_lo += p;
            }
        }
        cent.push(p_f / p_all);
        tilt.push(10.0 * (p_hi / p_lo.max(1e-12)).max(1e-12).log10());
        let logs: Vec<f32> = spec.iter().map(|s| s.ln()).collect();
        let gm = (logs.iter().sum::<f32>() / nb as f32).exp();
        flat.push(gm / (spec.iter().sum::<f32>() / nb as f32));
        flux.push(match &prev_log {
            Some(p) => {
                (logs.iter().zip(p).map(|(a, b)| (a - b) * (a - b)).sum::<f32>() / nb as f32).sqrt()
            }
            None => 0.0,
        });
        prev_log = Some(logs);

        // Высота голоса — автокорреляцией, 70…500 Гц.
        let a0: f32 = frame.iter().map(|x| x * x).sum();
        if a0 <= 0.0 {
            f0.push(0.0);
            conf.push(0.0);
        } else {
            let (lo, hi) = ((RATE / 500.0) as usize, (RATE / 70.0) as usize);
            let mut best = (lo, f32::MIN);
            for lag in lo..hi {
                let a = dot(&frame[..N - lag], &frame[lag..]);
                if a > best.1 {
                    best = (lag, a);
                }
            }
            f0.push(RATE / best.0 as f32);
            conf.push(best.1 / a0);
        }
        i += HOP;
    }

    let n = le.len();
    let le_max = le.iter().copied().fold(f32::MIN, f32::max);
    let act: Vec<bool> = le.iter().map(|v| *v > le_max - 25.0).collect();
    let any_act = act.iter().any(|a| *a);
    let w: Vec<bool> = if any_act { act.clone() } else { vec![true; n] };
    let sel = |v: &[f32]| -> Vec<f32> { v.iter().zip(&w).filter(|(_, m)| **m).map(|(x, _)| *x).collect() };

    let voiced: Vec<bool> = (0..n).map(|k| act[k] && conf[k] > 0.45).collect();
    let nv = voiced.iter().filter(|v| **v).count();
    let st: Vec<f32> = if nv >= 3 {
        (0..n)
            .filter(|&k| voiced[k])
            .map(|k| 12.0 * (f0[k] / 100.0).log2())
            .collect()
    } else {
        vec![0.0]
    };

    // Модуляция громкости 3–8 Гц (ритм смеха) — ДПФ огибающей.
    let mean_le = le.iter().sum::<f32>() / n as f32;
    let env: Vec<f32> = (0..n)
        .map(|k| {
            let hw = 0.5 - 0.5 * (2.0 * PI * k as f32 / (n.max(2) - 1) as f32).cos();
            (le[k] - mean_le) * hw
        })
        .collect();
    let df = 1.0 / (n as f32 * HOP as f32 / RATE);
    let (mut band, mut total) = (0.0f32, 0.0f32);
    for k in 0..=n / 2 {
        let f = k as f32 * df;
        let (mut r, mut m) = (0.0f32, 0.0f32);
        for (t, v) in env.iter().enumerate() {
            let ph = -2.0 * PI * (k * t) as f32 / n as f32;
            r += v * ph.cos();
            m += v * ph.sin();
        }
        let p = r * r + m * m;
        if f > 0.5 {
            total += p;
        }
        if (3.0..=8.0).contains(&f) {
            band += p;
        }
    }
    let modulation = 10.0 * (band / (total + 1e-9) + 1e-9).log10();
    let onsets = act.windows(2).filter(|p| !p[0] && p[1]).count() as f32;

    let le_w = sel(&le);
    let cent_w = sel(&cent);
    let conf_act: Vec<f32> = (0..n).filter(|&k| act[k]).map(|k| conf[k]).collect();
    out[0] = mean(&le_w);
    out[1] = std(&le_w);
    out[2] = le_max - percentile(&le, 0.10);
    out[3] = mean(&cent_w);
    out[4] = std(&cent_w);
    out[5] = mean(&sel(&tilt));
    out[6] = mean(&sel(&flat));
    out[7] = mean(&sel(&flux));
    out[8] = mean(&st);
    out[9] = std(&st);
    out[10] = if st.len() > 1 {
        st.iter().copied().fold(f32::MIN, f32::max) - st.iter().copied().fold(f32::MAX, f32::min)
    } else {
        0.0
    };
    out[11] = nv as f32 / n as f32;
    out[12] = if conf_act.is_empty() { 0.0 } else { mean(&conf_act) };
    out[13] = modulation;
    out[14] = onsets;
    out[15] = act.iter().filter(|a| **a).count() as f32 / n as f32;
    out
}

/// Скалярное произведение в 8 независимых суммах: компилятор раскладывает его на
/// векторные инструкции (в 5–8 раз быстрее простой суммы).
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut acc = [0.0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, y) in ca.zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    acc.iter().sum::<f32>() + tail
}

fn mean(v: &[f32]) -> f32 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f32>() / v.len() as f32
    }
}

fn std(v: &[f32]) -> f32 {
    let m = mean(v);
    (v.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / v.len().max(1) as f32).sqrt()
}

/// Перцентиль с линейной интерполяцией (как в numpy).
fn percentile(v: &[f32], q: f32) -> f32 {
    let mut s = v.to_vec();
    s.sort_by(f32::total_cmp);
    if s.is_empty() {
        return 0.0;
    }
    let pos = q * (s.len() - 1) as f32;
    let (i, f) = (pos as usize, pos.fract());
    if i + 1 < s.len() {
        s[i] + (s[i + 1] - s[i]) * f
    } else {
        s[i]
    }
}

/// Быстрое преобразование Фурье по основанию 2, на месте.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f32, secs: f32, amp: f32) -> Vec<f32> {
        (0..(secs * RATE) as usize)
            .map(|i| (2.0 * PI * f * i as f32 / RATE).sin() * amp)
            .collect()
    }

    #[test]
    fn fft_finds_bin() {
        let mut re: Vec<f32> = (0..512).map(|i| (2.0 * PI * 8.0 * i as f32 / 512.0).cos()).collect();
        let mut im = vec![0.0; 512];
        fft(&mut re, &mut im);
        let mag: Vec<f32> = (0..256).map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt()).collect();
        let peak = (0..256).max_by(|a, b| mag[*a].total_cmp(&mag[*b])).unwrap();
        assert_eq!(peak, 8);
    }

    #[test]
    fn pitch_of_200hz_tone() {
        let f = features(&tone(200.0, 2.0, 0.3));
        // 12·log2(200/100) = 12 полутонов
        assert!((f[8] - 12.0).abs() < 0.6, "pitch {}", f[8]);
        assert!(f[11] > 0.9, "voiced {}", f[11]);
    }

    #[test]
    fn louder_is_louder_and_finite() {
        let q = features(&tone(150.0, 2.0, 0.01));
        let l = features(&tone(150.0, 2.0, 0.5));
        assert!(l[0] > q[0] + 20.0);
        assert!(l.iter().chain(&q).all(|x| x.is_finite()));
    }

    #[test]
    fn laugh_rhythm_has_modulation() {
        // «ха-ха-ха»: 5 вспышек в секунду
        let t = tone(220.0, 2.0, 0.3);
        let burst: Vec<f32> = t
            .iter()
            .enumerate()
            .map(|(i, x)| if (i as f32 / RATE * 5.0).fract() < 0.5 { *x } else { 0.0 })
            .collect();
        let steady = features(&t);
        let laugh = features(&burst);
        assert!(laugh[13] > steady[13] + 3.0, "{} vs {}", laugh[13], steady[13]);
        assert!(laugh[14] >= 8.0, "onsets {}", laugh[14]);
    }
}
