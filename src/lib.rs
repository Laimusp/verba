//! Verba — диктовка для Windows: держишь хоткей → говоришь → текст встаёт в поле.
//! Библиотека — всё, кроме точки входа; тесты ходят сюда.

pub mod logger;

pub mod app;
pub mod audio;
pub mod cli;
pub mod config;
pub mod controller;
pub mod hotkey;
pub mod icons;
pub mod net;
pub mod paste;
pub mod secrets;
pub mod sound;
pub mod target;
pub mod tray;
