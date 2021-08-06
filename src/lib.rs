use anyhow::{anyhow, Result};
use ffmpeg::{
    codec, decoder, encoder, format, frame, picture, software::scaling, util::error, Packet,
    Rational,
};
use std::path::Path;
use std::{
    io::{Cursor, Read},
    result::Result as StdResult,
};
use zip::{read::ZipFile, ZipArchive};

pub struct Frame<S: AsRef<str>> {
    filename: S,
    delay: u32,
}

const MILLIS: i32 = 1_000;
const DECODER_TIME_BASE: Rational = Rational(1, MILLIS);
const OUTPUT_TIME_BASE: Rational = Rational(1, 90_000);

const LANCZOS: scaling::Flags = scaling::Flags::LANCZOS;
const YUV420P: format::Pixel = format::Pixel::YUV420P;

fn send_packet(
    decoder: &mut decoder::Opened,
    file: &mut ZipFile,
    timestamp: &mut i64,
    duration: i64,
    time_base: Rational,
) -> Result<()> {
    let mut packet = Packet::new(file.size() as usize);
    file.read_exact(packet.data_mut().unwrap())?;
    packet.set_flags(codec::packet::Flags::KEY);
    packet.set_pts(Some(*timestamp));
    packet.set_duration(duration);
    packet.rescale_ts(DECODER_TIME_BASE, time_base);
    *timestamp += duration;
    decoder.send_packet(&packet)?;
    Ok(())
}

fn send_frame(
    encoder: &mut encoder::video::Video,
    decoded: &frame::Video,
    scaler: &mut scaling::Context,
    scaled: &mut frame::Video,
) -> Result<()> {
    let src = scaler.input();
    let (src_format, src_w, src_h) = (decoded.format(), decoded.width(), decoded.height());
    if src_format != src.format || src_w != src.width || src_h != src.height {
        let dst = *scaler.output();
        scaler.cached(
            src_format, src_w, src_h, dst.format, dst.width, dst.height, LANCZOS,
        );
    }
    scaler.run(decoded, scaled)?;
    scaled.set_pts(decoded.timestamp());
    scaled.set_kind(picture::Type::None);
    encoder.send_frame(scaled)?;
    Ok(())
}

fn receive_packet(
    encoder: &mut encoder::video::Video,
    output: &mut format::context::Output,
    packet: &mut Packet,
    time_base: Rational,
) -> Result<()> {
    while wrap_result(encoder.receive_packet(packet))? {
        packet.rescale_ts(time_base, OUTPUT_TIME_BASE);
        packet.write_interleaved(output)?;
    }
    Ok(())
}

fn wrap_result(result: StdResult<(), error::Error>) -> Result<bool> {
    use error::{Error::*, EAGAIN};
    match result {
        Ok(()) => Ok(true),
        Err(Other { errno: EAGAIN }) | Err(Eof) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

pub fn convert_to_mp4<Z: AsRef<[u8]>, S: AsRef<str>, O: AsRef<Path>>(
    zip: Z,
    frames: &[Frame<S>],
    output_path: O,
) -> Result<()> {
    let mut archive = ZipArchive::new(Cursor::new(zip))?;
    let lowest_delay = frames
        .iter()
        .map(|f| f.delay)
        .min()
        .ok_or_else(|| anyhow!("Slide show with 0 frames?!"))? as i32;
    let mut frames = frames.iter();
    let frame = frames.next().unwrap();
    let decoder = &mut codec::Context::new().decoder().open_as(
        codec::decoder::find({
            let name = frame.filename.as_ref().as_bytes();
            if name.len() >= 3 && name[name.len() - 3..].eq_ignore_ascii_case(b"png") {
                codec::Id::PNG
            } else {
                codec::Id::MJPEG
            }
        })
        .ok_or_else(|| anyhow!("Couldn't find suitable decoder"))?,
    )?;
    let ts = &mut 0;
    let enc_tb = Rational(lowest_delay, MILLIS);
    send_packet(
        decoder,
        &mut archive.by_name(frame.filename.as_ref())?,
        ts,
        frame.delay as i64,
        enc_tb,
    )?;
    let decoded = &mut frame::Video::empty();
    let scaled = &mut frame::Video::empty();
    decoder.receive_frame(decoded)?;
    let (src_w, src_h) = (decoded.width(), decoded.height());
    let (dst_w, dst_h) = (src_w + src_w % 2, src_h + src_h % 2);
    let scaler = &mut scaling::Context::get(
        decoded.format(),
        src_w,
        src_h,
        YUV420P,
        dst_w,
        dst_h,
        LANCZOS,
    )?;
    let output = &mut format::output(&output_path)?;
    let mut stream = output.add_stream(
        codec::encoder::find(codec::Id::H264)
            .ok_or_else(|| anyhow!("Couldn't find suitable encoder"))?,
    )?;
    let mut encoder = stream.codec().encoder().video()?;
    encoder.set_flags(codec::Flags::GLOBAL_HEADER);
    encoder.set_width(dst_w);
    encoder.set_height(dst_h);
    encoder.set_frame_rate(Some(enc_tb.invert()));
    encoder.set_format(YUV420P);
    encoder.set_time_base(enc_tb);
    stream.set_parameters(
        encoder.open_with([("crf", "18"), ("preset", "veryslow")].iter().collect())?,
    );
    stream.set_time_base(OUTPUT_TIME_BASE);
    let encoder = &mut stream.codec().encoder().video()?;
    output.write_header()?;
    let packet = &mut Packet::empty();
    send_frame(encoder, decoded, scaler, scaled)?;
    receive_packet(encoder, output, packet, enc_tb)?;
    for Frame { filename, delay } in frames {
        send_packet(
            decoder,
            &mut archive.by_name(filename.as_ref())?,
            ts,
            *delay as i64,
            enc_tb,
        )?;
        while wrap_result(decoder.receive_frame(decoded))? {
            send_frame(encoder, decoded, scaler, scaled)?;
            receive_packet(encoder, output, packet, enc_tb)?;
        }
    }
    decoder.send_eof()?;
    while wrap_result(decoder.receive_frame(decoded))? {
        send_frame(encoder, decoded, scaler, scaled)?;
        receive_packet(encoder, output, packet, enc_tb)?;
    }
    encoder.send_eof()?;
    receive_packet(encoder, output, packet, enc_tb)?;
    output.write_trailer()?;
    Ok(())
}
