use std::convert::From;
use std::io;

#[derive(Debug)]
pub enum Error {
    Generic(&'static str),
    IoError(io::Error),
}

impl From<&'static str> for Error {
    fn from(s: &'static str) -> Self {
        Error::Generic(s)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::IoError(e)
    }
}