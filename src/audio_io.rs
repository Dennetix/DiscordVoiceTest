use std::{
    fmt::Debug,
    mem::MaybeUninit,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize, SampleRate, StreamConfig,
};
use ringbuf::{Consumer, Producer, SharedRb, StaticRb};
use tracing::error;

const SAMPLE_RATE: u32 = 48000;

const OUTPUT_BUFFER_SIZE: usize = 24576;
const INPUT_BUFFER_SIZE: usize = 2048;

type RbProducer = Producer<i16, Arc<SharedRb<i16, [MaybeUninit<i16>; OUTPUT_BUFFER_SIZE]>>>;
type RbConsumer = Consumer<i16, Arc<SharedRb<i16, [MaybeUninit<i16>; INPUT_BUFFER_SIZE]>>>;

pub struct AudioOutput {
    stopped: Arc<AtomicBool>,
    buffer_producer: RbProducer,
}

impl AudioOutput {
    pub fn new() -> Self {
        let config = StreamConfig {
            channels: 2,
            sample_rate: SampleRate(SAMPLE_RATE),
            buffer_size: BufferSize::Fixed(1024),
        };

        let (buffer_producer, mut buffer_consumer) =
            StaticRb::<i16, OUTPUT_BUFFER_SIZE>::default().split();

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_clone = stopped.clone();

        thread::spawn(move || {
            let device = cpal::default_host().default_output_device().unwrap();

            let stream = device
                .build_output_stream(
                    &config,
                    move |data: &mut [i16], _| {
                        for sample in data {
                            *sample = match buffer_consumer.pop() {
                                Some(s) => s,
                                None => 0,
                            };
                        }
                    },
                    |e| error!("{e}"),
                )
                .unwrap();

            stream.play().unwrap();

            while !stopped_clone.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
        });

        Self {
            stopped,
            buffer_producer,
        }
    }

    pub fn push(&mut self, data: &[i16]) {
        self.buffer_producer.push_slice(data);
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

impl Debug for AudioOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AudioOutput")
    }
}

pub struct AudioInput {
    stopped: Arc<AtomicBool>,
    buffer_consumer: RbConsumer,
}

impl AudioInput {
    pub fn new() -> Self {
        let config = StreamConfig {
            channels: 1,
            sample_rate: SampleRate(SAMPLE_RATE),
            buffer_size: BufferSize::Fixed(512),
        };

        let (mut buffer_producer, buffer_consumer) =
            StaticRb::<i16, INPUT_BUFFER_SIZE>::default().split();

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_clone = stopped.clone();

        thread::spawn(move || {
            let device = cpal::default_host().default_input_device().unwrap();

            let stream = device
                .build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        buffer_producer.push_slice(data);
                    },
                    |e| error!("{e}"),
                )
                .unwrap();

            stream.play().unwrap();

            while !stopped_clone.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
        });

        Self {
            stopped,
            buffer_consumer,
        }
    }

    pub fn pop(&mut self, data: &mut [i16]) {
        self.buffer_consumer.pop_slice(data);
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

impl Debug for AudioInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AudioInput")
    }
}
