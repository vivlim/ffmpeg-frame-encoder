extern crate ffmpeg_next as ffmpeg;
use thiserror::Error;
use std::{borrow::BorrowMut, cell::RefCell, convert::TryInto, path::{self, Path, PathBuf}, thread::{self, JoinHandle, Thread}, time::Duration};

use crossbeam_channel::{Receiver, SendError, Sender, TryRecvError};
use ffmpeg::{ChannelLayout, Rational, filter, format::Pixel, frame, util::format, Rescale};

use crate::{filters::{make_audio_filter, make_video_filter}, logger::{self, log_thread::{Event, HtmlTableLogger, LogError, LogMessage, LogSources, ThreadedLogger}}, sink::{AudioPlane, Frame, FrameData, RetroAVCollector, VideoPlane}};

#[derive(Debug, Clone)]
pub enum OutputArgs {
    AudioVideo(AudioArgs, VideoArgs),
    Video(VideoArgs),
    Audio(AudioArgs),
}

#[derive(Error, Debug)]
pub enum EncodeError {
    #[error("Failed to recieve message {0:?}")]
    ChannelRecvError(#[from] crossbeam_channel::TryRecvError),
    #[error("Failed to send message {0:?}")]
    ChannelSendError(#[from] crossbeam_channel::SendError<Frame<FrameData>>),
    #[error("IO error writing to file {0:?}")]
    IoError(#[from] std::io::Error),
    #[error("Error writing log {0:?}")]
    LogError(#[from] LogError),
    #[error("Failed to send log message to log thread {0:?}")]
    LogSendError(#[from] crossbeam_channel::SendError<LogMessage<LogSources>>),
    #[error("Ffmpeg error {0:?}")]
    FfmpegError(#[from] ffmpeg::Error),
    #[error("Undefined operation in flushing logic: {0}")]
    UndefinedOperationIndex(usize),

}

pub fn start_thread(receiver: Receiver<Frame<FrameData>>, path: PathBuf, log_path: Option<PathBuf>) -> JoinHandle<Result<(), EncodeError>> {
    let logger = match log_path {
        Some(path) => Some(HtmlTableLogger::<LogSources>::new(path)),
        None => None,
    };

    let mut encoder = CollectedAVFfmpegEncoder {
        receiver,
        video_path: path.into_boxed_path(),
        ffmpeg_context: None,
        is_ending: false,
        logger: match &logger {
            Some(logger) => Some(logger.get_sender()),
            None => None,
        }
    };

    thread::spawn(move || {
        let logger_handle = match logger {
            Some(mut logger) => Some((logger.begin(), logger.get_sender())),
            None => None,
        };
        match encoder.read_collector_to_end(){
            Ok(_) => (),
            Err(e) => eprintln!("Encode thread exited with error: {:?}", e),
        }
        match logger_handle {
            Some((joinhandle, channel)) => {
                channel.send(LogMessage::Eof)?;
                joinhandle.join().unwrap()
            },
            None => Ok(()),
        }?;
        return Ok(());
    })
}


pub struct CollectedAVFfmpegEncoder {
    pub receiver: Receiver<Frame<FrameData>>,

    video_path: Box<Path>,

    ffmpeg_context: Option<FfmpegContext>,

    is_ending: bool,
    logger: Option<Sender<LogMessage<LogSources>>>
}

#[derive(Debug, Clone)]
pub struct VideoArgs {
    pub pixel_format: Pixel,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct AudioArgs {
    pub sample_rate: u32,
}

struct FfmpegContext {
    pub octx: RefCell<ffmpeg::format::context::Output>,
    pub video: Option<FfmpegVideoContext>,
    pub audio: Option<FfmpegAudioContext>,
}

struct FfmpegVideoContext {
    pub encoder: ffmpeg::encoder::Video,
    pub filter: ffmpeg::filter::Graph,
    pub args: VideoArgs,
}

struct FfmpegAudioContext {
    pub encoder: ffmpeg::encoder::Audio,
    pub filter: ffmpeg::filter::Graph,
    pub args: AudioArgs,
}

impl FfmpegContext {
    pub fn new(output_args: OutputArgs, output_path: Box<Path>) -> Result<Self, ffmpeg::Error> {

        ffmpeg::log::set_level(ffmpeg::log::Level::Trace);
        ffmpeg::init()?;

        let mut octx = ffmpeg::format::output(&output_path)?;

        let video_context = match &output_args {
            OutputArgs::Video(video_args) | OutputArgs::AudioVideo(_, video_args) => {
                let detected_vcodec = octx.format().codec(&output_path, ffmpeg::media::Type::Video);
                println!("Guessing video codec {:?}", detected_vcodec);
                let vcodec = ffmpeg::encoder::find(detected_vcodec).unwrap().video()?;

                // set up output stream
                let mut output = octx.add_stream(vcodec)?;
                output.set_time_base(Rational::new(1, 60));

                // set up encoder
                let mut encoder = output.codec().encoder().video()?;
                encoder.set_bit_rate(2560000);
                // just use the first format...
                encoder.set_format(encoder.codec().unwrap().video()?.formats().unwrap().nth(0).unwrap());
                encoder.set_time_base(output.time_base());
                encoder.set_frame_rate(Some(Rational::new(video_args.fps.try_into().unwrap(), 1)));
                encoder.set_width(video_args.width);
                encoder.set_height(video_args.height);

                // create video filter
                let filter = make_video_filter(&encoder, &video_args)?;
                
                // turn the encoder context into an actual Encoder
                let encoder = encoder.open_as(vcodec)?;

                Some(FfmpegVideoContext {
                    encoder,
                    filter,
                    args: video_args.clone(),
                })
            },
            OutputArgs::Audio(_) => None
        };

        let audio_context = match &output_args {
            OutputArgs::Audio(audio_args) | OutputArgs::AudioVideo(audio_args, _) => {
                let detected_acodec = octx.format().codec(&output_path, ffmpeg::media::Type::Audio);

                println!("Guessing audio codec {:?}", detected_acodec);

                let acodec = ffmpeg::encoder::find(detected_acodec).unwrap().audio()?;

                // Audio
                // set up output stream
                let mut output = octx.add_stream(acodec)?;

                // set up encoder
                let mut encoder = output.codec().encoder().audio()?;
                encoder.set_bit_rate(640000);
                encoder.set_max_bit_rate(990000);
                encoder.set_rate(audio_args.sample_rate.try_into().unwrap());
                //audio_encoder.set_rate(44000)
                encoder.set_channels(2);
                encoder.set_channel_layout(ChannelLayout::STEREO);
                // just use the first format
                encoder.set_format(encoder.codec().unwrap().audio()?.formats().unwrap().nth(0).unwrap());

                /*
                output.set_time_base((1, 44100));
                encoder.set_time_base((1, 44100));
                */

                let mut encoder = encoder.open_as(acodec)?;
                let filter = make_audio_filter(&encoder, &audio_args)?;
                Some(FfmpegAudioContext {
                    encoder,
                    filter,
                    args: audio_args.clone()
                })
            },
            OutputArgs::Video(_) => None
        };

        octx.write_header()?;
        ffmpeg::format::context::output::dump(&octx, 0, None);

        Ok(FfmpegContext {
            octx: RefCell::new(octx),
            video: video_context,
            audio: audio_context,
        })
    }

}

enum OperationResult {
    Filter(Result<(), ffmpeg::Error>),
    Encode(Result<(), ffmpeg::Error>),
}

impl CollectedAVFfmpegEncoder {
    pub fn read_collector_to_end(&mut self) -> Result<(), EncodeError> {
        // ffmpeg operations which all return error code 11 when no data is available and should be repeatedly called until exit
        let mut ffmpeg_operations: [Option<fn(&mut CollectedAVFfmpegEncoder) -> Result<(), EncodeError>>; 4] = [None; 4];

        let mut eof_was_sent_to_encoders = false;

        loop {
            // No operations have been defined yet, try to set them based on the current context. ffmpeg context doesn't exist at the beginning so we have to do this
            // until one exists
            if let [None, None, None, None] = ffmpeg_operations {
                // Indices 0 and 1 are reserved for filters, 2 and 3 are reserved for encoders
                if let Some(FfmpegContext { octx: _, video: Some(_), audio: _ }) = &self.ffmpeg_context {
                    ffmpeg_operations[0] = Some(CollectedAVFfmpegEncoder::get_filtered_video_frame_and_start_encode);
                    ffmpeg_operations[2] = Some(CollectedAVFfmpegEncoder::write_encoded_video_packet);
                }

                if let Some(FfmpegContext { octx: _, video: _, audio: Some(_) }) = &self.ffmpeg_context {
                    ffmpeg_operations[1] = Some(CollectedAVFfmpegEncoder::get_filtered_audio_frame_and_start_encode);
                    ffmpeg_operations[3] = Some(CollectedAVFfmpegEncoder::write_encoded_audio_packet);
                }
            }

            match self.receiver.try_recv() {
                Ok(frame) => self.handle_frame(frame),
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(EncodeError::ChannelRecvError(TryRecvError::Disconnected)),
                Err(crossbeam_channel::TryRecvError::Empty) => Ok(())
            }?;

            let mut operation_results = [None, None, None, None];
            for operation_index in 0..ffmpeg_operations.len() {
                match ffmpeg_operations[operation_index] {
                    Some(operation) => { // operation is defined and can execute
                        match operation(self) {
                            Ok(_) => { break; }
                            Err(e @ EncodeError::FfmpegError(ffmpeg::Error::Other { errno: 11 /* temporarily unavailable, keep trying */ })) => {
                                operation_results[operation_index] = Some(e)
                            },
                            Err(EncodeError::FfmpegError(ffmpeg::Error::Eof)) => {
                                operation_results[operation_index] = Some(EncodeError::FfmpegError(ffmpeg::Error::Eof))
                            }
                            Err(e) => {
                                eprintln!("Error when encoding/writing (operation #{}): {}", operation_index, e);
                                return Err(e.into());
                            }
                        }

                    },
                    None => {
                        // For undefined operations, simulate them being at the end
                        // so I don't have to rewrite this extremely rigid flushing logic .
                        // which I really ought to.
                        operation_results[operation_index] = match operation_index {
                            0..=1 => Some(EncodeError::FfmpegError(ffmpeg::Error::Other { errno: 11 })), // If a filter operation is undefined, just treat it as if it was at the end.
                            2..=3 => Some(EncodeError::FfmpegError(ffmpeg::Error::Eof)), // And if an encoder is undefined, treat it as if it is at eof
                            i => { return Err(EncodeError::UndefinedOperationIndex(i)) }
                        }
                    }

                }
            }

            // If the ending flag is set, we need to see which end conditions are met.
            if self.is_ending && self.receiver.is_empty() {
                // No more frames coming from the source, but we can't send eof to the encoders until the filters are drained.
                match operation_results {
                    [Some(EncodeError::FfmpegError(ffmpeg::Error::Other { errno: 11 })),
                    Some(EncodeError::FfmpegError(ffmpeg::Error::Other { errno: 11 })),
                    Some(EncodeError::FfmpegError(ffmpeg::Error::Eof)),
                    Some(EncodeError::FfmpegError(ffmpeg::Error::Eof))] => { // Both encoders are finished.
                        // Both graphs are out of data, and both encoders are at the end of the file.
                        if let Some(ffmpeg_context) = &mut self.ffmpeg_context {
                            ffmpeg_context.octx.get_mut().write_trailer()?;
                            println!("wrote trailer");
                        }
                        break; // Exit the loop
                    },
                    [Some(EncodeError::FfmpegError(ffmpeg::Error::Other { errno: 11 })),
                    Some(EncodeError::FfmpegError(ffmpeg::Error::Other { errno: 11 })),
                    _, _] => { // Both filters are out of data to process
                        // Both graphs are out of data, but encoders aren't done yet.
                        // Send one EOF to each encoder.
                        if !eof_was_sent_to_encoders {
                            let mut succeeded = true;
                            if let Some(FfmpegContext{ video: Some(video_context), .. }) = &mut self.ffmpeg_context {
                                match video_context.encoder.send_eof() {
                                    Err(ffmpeg::Error::Other { errno: 11 /* temporarily unavailable */}) => {
                                        println!("eof for video failed (temporarily unavailable)");
                                        succeeded = false;
                                    },
                                    Err(ffmpeg::Error::Eof) => {
                                        println!("video is already eof");
                                        succeeded = succeeded && true;
                                    },
                                    Ok(_) => { succeeded = succeeded && true; }
                                    Err(e) => {
                                        panic!("error when sending video eof: {}", e);
                                    }
                                }
                            }
                            if let Some(FfmpegContext{ audio: Some(audio_context), .. }) = &mut self.ffmpeg_context {
                                match audio_context.encoder.send_eof() {
                                    Err(ffmpeg::Error::Other { errno: 11 /* temporarily unavailable */}) => {
                                        println!("eof for audio failed (temporarily unavailable)");
                                        succeeded = false;
                                    },
                                    Err(ffmpeg::Error::Eof) => {
                                        println!("audio is already eof");
                                        succeeded = succeeded && true;
                                    },
                                    Ok(_) => { succeeded = succeeded && true; }
                                    Err(e) => {
                                        panic!("error when sending audio eof: {}", e);
                                    }
                                }
                            }
                            if succeeded {
                                eof_was_sent_to_encoders = true;
                            }
                        }
                    },
                    _ => () // Any other combination doesn't matter
                }

            }
        }
        Ok(())
    }

    pub fn handle_frame(&mut self, frame: Frame<FrameData>) -> Result<(), EncodeError> {
        //println!("Handling frame kind {:?}", frame.data);
        let frame_number = frame.frame_number;
        match (&mut self.ffmpeg_context, frame.data, &self.logger) {
            (Some(FfmpegContext { video: Some(video_context), .. }), FrameData::Video(vplane), logger) => {
                let mut frame = frame_from_video_plane(&vplane, video_context);
                frame.set_pts(Some(frame_number as i64));
                // push frame to filter
                println!("frame pushed to filter");
                write_log(logger, LogSources::Sink, format!("Video frame {}, pts {}", frame_number, frame_number))?;
                video_context.filter.get("in").unwrap().source().add(&frame)?;
            },

            (Some(FfmpegContext { audio: Some(audio_context), octx, .. }), FrameData::Audio(aplane), logger) => {
                let mut frame = frame_from_audio_plane(&aplane, audio_context);

                /*
                let new_pts = unsafe {
                    ffmpeg::sys::av_rescale_q(
                        frame_number as i64,
                        Rational(1, 60).into(),
                        octx.borrow().stream(1).unwrap().time_base().into()
                    )
                };
                frame.set_pts(Some(new_pts));*/
                frame.set_pts(Some(frame_number as i64));
                // push frame to filter
                write_log(logger, LogSources::Sink, format!("Audio frame {}", frame_number))?;
                audio_context.filter.get("in").unwrap().source().add(&frame)?;
            },
            (None, FrameData::Configure(output_args), logger) => {
                // Create a new ffmpeg context using the provided config.
                write_log(logger, LogSources::Sink, format!("Configure frame {}, {:?}", frame_number, output_args))?;
                match FfmpegContext::new(output_args, self.video_path.clone()) {
                    Ok(context) => {
                        self.ffmpeg_context = Some(context);
                    }
                    Err(e) => {
                        eprintln!("Failed to set up ffmpeg context: {}", e);
                    }
                }
            },

            (Some(ffmpeg_context), FrameData::Configure(output_args), logger) => {
                println!("Reconfiguring after a ffmpeg context already exists is not implemented.");
                write_log(logger, LogSources::Sink, format!("Rejected configure frame {}, {:?}", frame_number, output_args))?;
            }

            (Some(ffmpeg_context), FrameData::End, logger) => {
                // stop processing frames
                write_log(logger, LogSources::Sink, format!("Stop processing frames @ {}", frame_number))?;
                self.is_ending = true;
            }, 

            _ => {
                panic!("unhandled case");
            }
        };
        Ok(())
    }

    fn get_filtered_video_frame_and_start_encode(&mut self) -> Result<(), EncodeError> {
        match (&mut self.ffmpeg_context, &self.logger) {
            (Some(FfmpegContext { video: Some(video_context), .. }), logger) => {
                let mut filtered_vframe = frame::Video::empty();
                match video_context.filter.get("out").unwrap().sink().frame(&mut filtered_vframe) {
                    Ok(..) => {
                        write_log(logger, LogSources::Filter, format!("Video frame pts {:?}", filtered_vframe.pts()))?;
                        eprintln!("🎥 Got filtered video frame {}x{} pts {:?}", filtered_vframe.width(), filtered_vframe.height(), filtered_vframe.pts());
                        if video_context.filter.get("in").unwrap().source().failed_requests() > 0 {
                            println!("🎥 failed to put filter input frame");
                        }
                        video_context.encoder.send_frame(&filtered_vframe)?/* ?*/;
                        Ok(())
                    },
                    Err(e) => Err(e.into())
                }
            },
            (Some(FfmpegContext { video: None, .. }), _) => Ok(()), // No-op when we aren't doing video
            (None, _) => { panic!("Shouldn't try to encode when there is no ffmpeg context"); }
        }
    }


    fn get_filtered_audio_frame_and_start_encode(&mut self) -> Result<(), EncodeError> {
        match (&mut self.ffmpeg_context, &self.logger) {
            (Some(FfmpegContext { audio: Some(audio_context), .. }), logger) => {
                let mut filtered_aframe = frame::Audio::empty();
                match audio_context.filter.get("out").unwrap().sink().frame(&mut filtered_aframe) {
                    Ok(..) => {
                        write_log(logger, LogSources::Filter, format!("Audio frame pts {:?}", filtered_aframe.pts()))?;
                        eprintln!("🔊 Got filtered audio frame {:?} pts {:?}", filtered_aframe, filtered_aframe.pts());
                        if audio_context.filter.get("in").unwrap().source().failed_requests() > 0 {
                            println!("🎥 failed to put filter input frame");
                        }

                        audio_context.encoder.send_frame(&filtered_aframe)?/*?*/;
                        Ok(())
                    },
                    Err(e) => Err(e.into())
                }
            },
            (Some(FfmpegContext { audio: None, .. }), _) => Ok(()), // No-op when we aren't doing audio
            (None, _) => { panic!("Shouldn't try to encode when there is no ffmpeg context"); }
        }
    }

    fn write_encoded_video_packet(&mut self) -> Result<(), EncodeError>{
        match (&mut self.ffmpeg_context, &self.logger) {
            (Some(FfmpegContext { video: Some(video_context), octx, .. }), logger) => {
                let mut encoded_packet = ffmpeg::Packet::empty();
                match video_context.encoder.receive_packet(&mut encoded_packet) {
                    Ok(..) => {
                        encoded_packet.set_stream(0);
                        write_log(logger, LogSources::Encoder, format!("Video packet pts {:?} dts {:?}", encoded_packet.pts(), encoded_packet.dts()))?;
                        eprintln!("📦 Writing packet, pts {:?} dts {:?} size {}", encoded_packet.pts(), encoded_packet.dts(), encoded_packet.size());
                        let octx = octx.get_mut();
                        encoded_packet.rescale_ts(Rational(1, video_context.args.fps as i32), octx.stream(0).unwrap().time_base());
                        eprintln!("📦 rescaled , pts {:?} dts {:?} size {}", encoded_packet.pts(), encoded_packet.dts(), encoded_packet.size());
                        match encoded_packet.write_interleaved(octx) {
                            Ok(..) => Ok(()),
                            Err(e) => {
                                eprintln!("Error writing encoded video packet: {}", e);
                                Err(e.into())
                            },
                        }
                    },
                    Err(e) => Err(e.into())
                }
            },
            (Some(FfmpegContext { video: None, .. }), _) => Ok(()), // No-op when we aren't doing video
            (None, _) => { panic!("Shouldn't try to write encoded packets when there is no ffmpeg context"); }
        }
    }
    fn write_encoded_audio_packet(&mut self) -> Result<(), EncodeError>{
        match (&mut self.ffmpeg_context, &self.logger) {
            (Some(FfmpegContext { audio: Some(audio_context), octx, .. }), logger) => {
                let mut encoded_packet = ffmpeg::Packet::empty();
                match audio_context.encoder.receive_packet(&mut encoded_packet) {
                    Ok(..) => {
                        encoded_packet.set_stream(1);
                        write_log(logger, LogSources::Encoder, format!("Audio packet pts {:?} dts {:?}", encoded_packet.pts(), encoded_packet.dts()));
                        eprintln!("📦 Writing audio packet, pts {:?} dts {:?} size {}", encoded_packet.pts(), encoded_packet.dts(), encoded_packet.size());
                        match encoded_packet.write_interleaved(octx.get_mut()) {
                            Ok(..) => Ok(()),
                            Err(e) => {
                                eprintln!("Error writing encoded audio packet: {}", e);
                                Err(e.into())
                            },
                        }
                    },
                    Err(e) => Err(e.into())
                }
            },
            (Some(FfmpegContext { audio: None, .. }), _) => Ok(()), // No-op when we aren't doing audio
            (None, _) => { panic!("Shouldn't try to write encoded packets when there is no ffmpeg context"); }
        }
    }

}


fn frame_from_video_plane(vplane: &VideoPlane, video_context: &mut FfmpegVideoContext) -> ffmpeg::frame::Video {
    let mut vframe = ffmpeg::frame::Video::new(video_context.args.pixel_format, vplane.width as u32, vplane.height as u32);
        let stride = vframe.stride(0);
        let pitch = vplane.pitch;

        let vframe_plane = vframe.data_mut(0);
        if vplane.data.len() == vframe_plane.len() && pitch == stride {
            vframe_plane.copy_from_slice(&vplane.data);
        } else {
            for y in 0..(vplane.height as usize) {
                let ffbegin = y * stride;
                let lrbegin = y * pitch;
                let min = usize::min(stride, pitch);
                vframe_plane[ffbegin..(ffbegin + min)].copy_from_slice(
                    &vplane.data[lrbegin..(lrbegin + min)]
                );
            }  
        }
        vframe
}

fn frame_from_audio_plane(aplane: &AudioPlane, audio_context: &mut FfmpegAudioContext) -> ffmpeg::frame::Audio {
    let mut aframe = frame::Audio::new(
        format::Sample::I16(format::sample::Type::Packed),
        aplane.data.len(),
        ChannelLayout::STEREO
    );
    aframe.set_channels(2);
    aframe.set_rate(audio_context.args.sample_rate);

    let aframe_plane = aframe.plane_mut(0);
    aframe_plane.copy_from_slice(aplane.data.as_slice());
    aframe
}


fn write_log(logger: &Option<Sender<LogMessage<LogSources>>>, source: LogSources, message: String) -> Result<(), SendError<LogMessage<LogSources>>> {
    if let Some(logger) = logger {
        logger.send(LogMessage::Event(Event::<LogSources> {
            source,
            description: message,
        }))?;
    }
    Ok(())
}