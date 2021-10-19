extern crate ffmpeg_next as ffmpeg;

use ffmpeg::{codec::audio, filter};

use crate::encoder::{AudioArgs, VideoArgs};

pub fn make_video_filter(
    video_encoder: &ffmpeg::encoder::video::Video,
    video_args: &VideoArgs
) -> Result<filter::Graph, ffmpeg::Error> {

    let pixel_format_string = match video_args.pixel_format {
        ffmpeg::format::Pixel::BGRA => "bgra",
        ffmpeg::format::Pixel::RGB555 => if cfg!(target_endian = "big") { "rgb555be" } else { "rgb555le" },
        ffmpeg::format::Pixel::RGB32 => "argb",
        ffmpeg::format::Pixel::RGB565 => if cfg!(target_endian = "big") { "rgb565be" } else { "rgb565le" },
        _ => {panic!("need to build pixel format strings in a more general way.");}
    };

    let pixel_aspect = 1; // assume square pixels for now...

    let mut video_filter = filter::Graph::new();

    let args = format!(
        "width={}:height={}:pix_fmt={}:frame_rate={}:pixel_aspect={}:time_base=1/{}",
        video_args.width,
        video_args.height,
        pixel_format_string,
        video_args.fps,
        pixel_aspect,
        video_args.fps,
    );
    eprintln!("🎥 filter args: {}", args);

    video_filter.add(&filter::find("buffer").unwrap(), "in", &args)?;
    //scale?
    video_filter.add(&filter::find("buffersink").unwrap(), "out", "")?;

    {
        let mut out = video_filter.get("out").unwrap();
        out.set_pixel_format(video_encoder.format());
    }

    video_filter.output("in", 0)?
        .input("out", 0)?
        .parse("null")?; // passthrough filter for video

    video_filter.validate()?;
    // human-readable filter graph
    eprintln!("{}", video_filter.dump());

    Ok(video_filter)
}

pub fn make_audio_filter(
    audio_encoder: &ffmpeg::codec::encoder::Audio,
    audio_args: &AudioArgs
) -> Result<filter::Graph, ffmpeg::Error> {
    let mut afilter = filter::Graph::new();
    let args = format!("time_base=1/{}:sample_rate={}:sample_fmt=s16:channel_layout=stereo", audio_args.sample_rate, audio_args.sample_rate);
    eprintln!("🔊 filter args: {}", args);
    afilter.add(&filter::find("abuffer").unwrap(), "in", &args)?;
    //aresample?
    afilter.add(&filter::find("abuffersink").unwrap(), "out", "")?;

    {
        let mut in_f = afilter.get("in").unwrap();
        //in_f.set_sample_format(audio_args.format());
        //in_f.set_channel_layout(audio_encoder.channel_layout());
        in_f.set_sample_rate(audio_args.sample_rate);
    }
    {
        let mut out = afilter.get("out").unwrap();
        out.set_sample_format(audio_encoder.format());
        out.set_channel_layout(audio_encoder.channel_layout());
        out.set_sample_rate(audio_encoder.rate());
    }

    afilter.output("in", 0)?
        .input("out", 0)?
        .parse(&format!("volume={}", audio_args.volume))?;
    afilter.validate()?;
    // human-readable filter graph
    eprintln!("{}", afilter.dump());

    if let Some(codec) = audio_encoder.codec() {
        if !codec
            .capabilities()
            .contains(ffmpeg::codec::capabilities::Capabilities::VARIABLE_FRAME_SIZE)
        {
            eprintln!("setting constant frame size {}", audio_encoder.frame_size());
            afilter
                .get("out")
                .unwrap()
                .sink()
                .set_frame_size(audio_encoder.frame_size());
        }
    }

    Ok(afilter)
}