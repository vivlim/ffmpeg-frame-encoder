use strum::IntoEnumIterator;
use thiserror::Error;
use std::{fs::File, io::Write, path::PathBuf, thread::JoinHandle};
use strum_macros::{EnumIter, AsRefStr};
use std::thread;

use crossbeam_channel::{Receiver, Sender};

#[derive(EnumIter, AsRefStr, Debug, Clone, Copy, PartialEq)]
pub enum LogSources {
    Sink,
    Filter,
    Encoder,
}

pub enum LogMessage<T> {
    Event(Event<T>),
    Eof
}
pub struct Event<T> {
    pub source: T,
    pub description: String
}

#[derive(Error, Debug)]
pub enum LogError {
    #[error("Failed to recieve message")]
    ChannelRecvError(#[from] crossbeam_channel::RecvError),
    #[error("IO error writing to file")]
    IoError(#[from] std::io::Error),

}


pub struct HtmlTableLogger<T> {
    input: Sender<LogMessage<T>>,
    output: Receiver<LogMessage<T>>,
    headings: Vec<(T, String)>,
    destination: PathBuf
}

pub trait ThreadedLogger<T> {
    fn get_sender(&self) -> Sender<LogMessage<T>>;
    fn begin(&mut self) -> JoinHandle<Result<(), LogError>>;
}

impl<T> HtmlTableLogger<T>
where T: IntoEnumIterator + AsRef<str> + Copy {
    pub fn new(destination: PathBuf) -> Self {
        let channel = crossbeam_channel::unbounded();
        Self { 
            input: channel.0,
            output: channel.1,
            headings: T::iter().map(|e| (e, String::from(e.as_ref()))).collect(),
            destination
        }
    }
}

impl<T> ThreadedLogger<T> for HtmlTableLogger<T>
where T: IntoEnumIterator + AsRef<str> + Clone + Copy + Send + PartialEq + 'static {
    fn get_sender(&self) -> Sender<LogMessage<T>> {
        self.input.clone()
    }

    fn begin(&mut self) -> JoinHandle<Result<(), LogError>> {
        let headings = self.headings.clone();
        let messages = self.output.clone();
        let destination = self.destination.clone();
        thread::spawn(move || {
            let mut file = File::create(destination)?;
            file.write(r#"<!DOCTYPE html>
<html>
<body>
<table border="1">
<tr>"#.as_bytes())?;
            for col in T::iter() {
                file.write(format!("<th>{}</th>\n", col.as_ref()).as_bytes())?;
            }
            file.write("</tr>\n".as_bytes())?;
            loop {
                let message = messages.recv()?;
                match message {
                    LogMessage::Event(event) => {
                        file.write("<tr>\n".as_bytes())?;
                        let col_idx =  headings.iter().position(|h| h.0 == event.source).expect("Column should be an existing one");
                        for _ in 0..col_idx {
                            file.write("<td></td>".as_bytes())?;
                        }
                        file.write(format!("<td>{}</td>", event.description).as_bytes())?;
                        for _ in 0..(headings.len() - col_idx - 1) {
                            file.write("<td></td>".as_bytes())?;
                        }
                        file.write("</tr>\n".as_bytes())?;

                    }
                    LogMessage::Eof => {
                        file.write("</table></body></html>".as_bytes())?;
                        return Ok(());
                    }
                }
            }
        })
    }
}