extern crate env_logger;
extern crate monome;
extern crate rand;
extern crate num;
use rand::prelude::*;
use std::{thread, time};

use monome::{Monome, MonomeEvent, KeyDirection};

fn toidx(x: i32, y: i32, width: i32) -> usize {
  (y * width + x) as usize
}

fn main() {
    env_logger::init();

    let mut monome = Monome::new("/prefix".to_string()).unwrap();

    println!("{:?}", monome);

    let mut v: Vec<u8> = vec![0; 64];
    let mut v2 = vec![false; 64];

    loop {
        for i in 0..64 {
            v[i] = (random::<u8>() % 16) as u8;
            v2[i] = if random::<u8>() % 2 == 0 { false } else { true };
        }
        // random intensity from 0 to 15
        monome.map(0, 0, &v);
        // On/Off
        monome.map(8, 0, &v2);

        let refresh = time::Duration::from_millis(33);
        thread::sleep(refresh);
    }
}