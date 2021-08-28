use crossbeam_channel::{Receiver, SendError, Sender};

use crate::encoder::OutputArgs;

pub struct Sink<T> {
    pub input: Sender<T>,
    pub output: Receiver<T>,
}

impl Default for Sink<Frame<FrameData>> {
    fn default() -> Self {
        let channel = crossbeam_channel::unbounded();
        Sink {
            input: channel.0,
            output: channel.1
        }
    }
}

pub struct Frame<T> {
    pub data: T,
    pub frame_number: u64,
}

#[derive(Debug)]
pub enum FrameData {
    Video(VideoPlane),
    Audio(AudioPlane),
    Configure(OutputArgs),
    End,
}

pub struct RetroAVCollector {
    pub sink: Sink<Frame<FrameData>>,

    audio_buf: Vec<(i16, i16)>, // accumulate audio for slicing into planes
}

#[derive(Debug)]
pub struct VideoPlane {
    pub data: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub pitch: usize,
}

#[derive(Debug)]
pub struct AudioPlane {
    pub data: Vec<(i16, i16)>
}

impl RetroAVCollector {
    pub fn new() -> Self{
        RetroAVCollector {
            sink: Default::default(),
            audio_buf: Default::default(),
        }
    }

    pub fn configure(&mut self, output_args: &OutputArgs, frame_number: u64) -> Result<(), SendError<Frame<FrameData>>> {
        self.sink.input.send(Frame {
            data: FrameData::Configure(output_args.clone()),
            frame_number,
        })
    }

    pub fn on_video_refresh(&mut self, data: &[u8], width: u32, height: u32, pitch: u32, frame_number: u64) -> Result<(), SendError<Frame<FrameData>>> {
        let plane = VideoPlane {
            data: data.to_vec(),
            width: width as usize,
            height: height as usize,
            pitch: pitch as usize
        };
        let frame = Frame {
            data: FrameData::Video(plane),
            frame_number
        };
        self.sink.input.send(frame)
    }

    pub fn on_audio_sample(&mut self, left: i16, right: i16, frame_number: u64) {
        self.audio_buf.push((left, right));
    }

    pub fn on_audio_sample_batch(&mut self, stereo_pcm: &[i16], frame_number: u64) -> usize {
        let left_iter = stereo_pcm.iter().step_by(2).cloned();
        let right_iter = stereo_pcm.iter().skip(1).step_by(2).cloned();
        self.audio_buf.extend(Iterator::zip(left_iter, right_iter));
        self.send_audio_plane_if_ready(frame_number).unwrap();
        stereo_pcm.len()
    }

    fn send_audio_plane_if_ready(&mut self, frame_number: u64) -> Result<(), SendError<Frame<FrameData>>> {
        // current code crams the entire buffer into a plane if it's ready
        // should i use sample rate here?
        // current code ends up collecting ~735 samples on picodrive
        let data = self.audio_buf.clone();
        let plane = AudioPlane {
            data
        };
        self.audio_buf.clear();
        let frame = Frame {
            data: FrameData::Audio(plane),
            frame_number,
        };
        self.sink.input.send(frame)
    }

    pub fn end(&mut self, frame_number: u64) -> Result<(), SendError<Frame<FrameData>>>{
        self.sink.input.send(Frame{
            data: FrameData::End,
            frame_number,
        })
    }
}