use crate::cw::oscillator::SAMPLE_RATE;
use pipewire::{self as pw, properties::properties, spa};
use std::{cell::RefCell, rc::Rc, time::Duration};
type Error = Box<dyn std::error::Error>;

/// Native playback; callbacks run on the main loop, without RT_PROCESS.
pub fn play(samples: Vec<f32>, target: Option<&str>, manual: bool) -> Result<(), Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let failure = Rc::new(RefCell::new(None::<String>));
    let quit = mainloop.clone();
    let err = failure.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, _, message| {
            *err.borrow_mut() = Some(message.to_owned());
            quit.quit();
        })
        .register();
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => "morse-tx",
        *pw::keys::NODE_DESCRIPTION => "cw-chat Morse TX",
        *pw::keys::AUDIO_CHANNELS => "1",
        "node.rate" => "1/48000",
    };
    if let Some(target) = target {
        props.insert("target.object", target);
    }
    let stream = pw::stream::StreamBox::new(&core, "morse-tx", props)?;
    let timeout = Duration::from_secs_f64(samples.len() as f64 / SAMPLE_RATE as f64 + 120.0);
    let quit = mainloop.clone();
    let err = failure.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        *err.borrow_mut() = Some("Playback timed out; check PipeWire routing".into());
        quit.quit();
    });
    timer.update_timer(Some(timeout), None).into_result()?;
    let quit = mainloop.clone();
    let err = failure.clone();
    let drained_quit = mainloop.clone();
    let process_quit = mainloop.clone();
    let process_error = failure.clone();
    let mut cursor = 0usize;
    let mut draining = false;
    let _listener = stream
        .add_local_listener_with_user_data(samples)
        .state_changed(move |_, _, _, state| {
            if let pw::stream::StreamState::Error(message) = state {
                *err.borrow_mut() = Some(message);
                quit.quit();
            }
        })
        .drained(move |_, _| drained_quit.quit())
        .process(move |stream, samples| {
            if draining {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let frames = if let Some(bytes) = data.data() {
                let capacity = bytes.len() / 4;
                let frames = if requested == 0 {
                    capacity
                } else {
                    capacity.min(requested)
                }
                .min(samples.len() - cursor);
                for (out, sample) in bytes[..frames * 4]
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(&samples[cursor..cursor + frames])
                {
                    out.copy_from_slice(&sample.to_le_bytes());
                }
                frames
            } else {
                0
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 4;
            *chunk.size_mut() = (frames * 4) as u32;
            cursor += frames;
            // Drop queues the final buffer before requesting a drain.
            drop(buffer);
            if cursor == samples.len() {
                draining = true;
                if let Err(error) = stream.flush(true) {
                    *process_error.borrow_mut() = Some(error.to_string());
                    process_quit.quit();
                }
            }
        })
        .register()?;
    let bytes = format_pod()?;
    let mut params = [spa::pod::Pod::from_bytes(&bytes).ok_or("Invalid audio format")?];
    let mut flags = pw::stream::StreamFlags::MAP_BUFFERS;
    if !manual {
        flags |= pw::stream::StreamFlags::AUTOCONNECT;
    }
    stream.connect(spa::utils::Direction::Output, None, flags, &mut params)?;
    mainloop.run();
    if let Some(message) = failure.borrow_mut().take() {
        return Err(message.into());
    }
    Ok(())
}

/// Serialized EnumFormat for mono 48 kHz F32LE, shared by playback and capture streams.
pub(crate) fn format_pod() -> Result<Vec<u8>, Error> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(1);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    positions[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(positions);
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|e| format!("Could not serialize audio format: {e:?}"))?
    .0
    .into_inner())
}
