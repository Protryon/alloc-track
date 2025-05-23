use std::{
    alloc::System,
    sync::mpsc,
    time::{Duration, Instant},
};

use alloc_track::{AllocTrack, BacktraceMode};

#[global_allocator]
static GLOBAL_ALLOC: AllocTrack<System> = AllocTrack::new(System, BacktraceMode::Short);

fn main() {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("thread2".to_string())
        .spawn(move || thread(receiver))
        .unwrap();

    let mut last_print = Instant::now();
    loop {
        let buf = vec![1u8; 1024];
        sender.send(buf).ok();
        std::thread::sleep(Duration::from_millis(100));
        if last_print.elapsed() > Duration::from_secs(10) {
            last_print = Instant::now();
            let report = alloc_track::backtrace_report(|_, _| true);
            println!("BACKTRACES\n{report}");
            println!("BACKTRACES CSV\n{}", report.csv());
            let report = alloc_track::thread_report();
            println!("THREADS\n{report}");
        }
    }
}

fn thread(receiver: mpsc::Receiver<Vec<u8>>) {
    let mut flip = false;
    while let Ok(block) = receiver.recv() {
        if flip {
            drop(block);
        } else {
            std::mem::forget(block);
        }
        flip = !flip;
    }
}
