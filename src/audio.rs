//! Захват микрофона через cpal (WASAPI). Колбэк только сводит каналы в моно и кладёт
//! сэмплы в lock-free кольцевой буфер; вся обработка — в потоке анализа (`engine.rs`).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::engine::{Ctrl, Engine};
use crate::win;

/// Живой поток захвата; drop останавливает запись.
pub struct Capture {
    _stream: cpal::Stream,
    pub device_name: String,
}

pub fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|d| d.description().ok().map(|x| x.name().to_owned()))
        .collect()
}

/// Запускает захват. Если выбранного устройства нет, берётся устройство по умолчанию
/// (тогда второе значение — пояснение для статуса).
pub fn start(device: Option<&str>, rec: &Engine) -> Result<(Capture, Option<String>), String> {
    let host = cpal::default_host();
    let mut note = None;
    let named = device.and_then(|want| {
        host.input_devices()
            .ok()?
            .find(|d| d.description().is_ok_and(|x| x.name() == want))
    });
    let dev = match (device, named) {
        (_, Some(d)) => d,
        (wanted, None) => {
            if let Some(w) = wanted {
                note = Some(format!("Нет «{w}», взят микрофон по умолчанию"));
            }
            host.default_input_device()
                .ok_or("Не найдено ни одного микрофона")?
        }
    };
    let device_name = dev
        .description()
        .map(|x| x.name().to_owned())
        .unwrap_or_default();
    let supported = dev
        .default_input_config()
        .map_err(|e| format!("Микрофон недоступен: {e}"))?;
    let config = supported.config();
    let rate = config.sample_rate;

    // 2 секунды запаса: оценка звука занимает десятки миллисекунд, звук копится здесь.
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(rate as usize * 2);
    let worker = rec.thread();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&dev, &config, producer, worker),
        SampleFormat::I16 => build::<i16>(&dev, &config, producer, worker),
        SampleFormat::I32 => build::<i32>(&dev, &config, producer, worker),
        SampleFormat::U16 => build::<u16>(&dev, &config, producer, worker),
        f => return Err(format!("Неподдерживаемый формат звука: {f:?}")),
    }
    .map_err(|e| format!("Не удалось открыть микрофон: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("Не удалось запустить микрофон: {e}"))?;
    rec.send(Ctrl::Source { consumer, rate });
    Ok((
        Capture {
            _stream: stream,
            device_name,
        },
        note,
    ))
}

fn build<T>(
    dev: &cpal::Device,
    config: &StreamConfig,
    mut producer: rtrb::Producer<f32>,
    worker: std::thread::Thread,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;
    // Громкость каждого канала (сглаженная). Микрофон часто подключён только в один вход
    // звуковой карты (Focusrite «Analogue 1 + 2»): если усреднять с пустым вторым каналом,
    // голос теряет половину громкости. Поэтому смешиваем только каналы, где есть звук.
    let mut energy = vec![0.0f32; channels];
    let mut use_ch = vec![true; channels];
    let capacity = producer.buffer().capacity();
    // Будим поток анализа ~40 раз в секунду: рот должен открываться без заметной задержки.
    let wake_at = (config.sample_rate as usize / 40).min(capacity / 2);
    dev.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _| {
            if channels > 1 {
                let n = (data.len() / channels).max(1) as f32;
                for (c, e) in energy.iter_mut().enumerate() {
                    let cur: f32 = data
                        .iter()
                        .skip(c)
                        .step_by(channels)
                        .map(|s| {
                            let v = s.to_sample::<f32>();
                            v * v
                        })
                        .sum::<f32>()
                        / n;
                    *e = *e * 0.95 + cur * 0.05;
                }
                let max = energy.iter().copied().fold(0.0f32, f32::max);
                for (u, e) in use_ch.iter_mut().zip(&energy) {
                    // в пределах 12 дБ от самого громкого канала
                    *u = max <= 0.0 || *e >= max * 0.063;
                }
            }
            let used = use_ch.iter().filter(|u| **u).count().max(1) as f32;
            for frame in data.chunks_exact(channels) {
                let mut sum = 0.0f32;
                for (&s, &u) in frame.iter().zip(&use_ch) {
                    if u {
                        sum += s.to_sample::<f32>();
                    }
                }
                // Буфер полон — значит, анализ не успевает; лишнее просто теряем.
                let _ = producer.push(sum / used);
            }
            if capacity - producer.slots() >= wake_at {
                worker.unpark();
            }
        },
        |e| {
            if matches!(
                e.kind(),
                ErrorKind::DeviceNotAvailable
                    | ErrorKind::StreamInvalidated
                    | ErrorKind::HostUnavailable
            ) {
                win::notify_main(win::EV_STREAM_ERROR);
            }
        },
        None,
    )
}
