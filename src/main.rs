use log::debug;
use log::error;
use midly::Smf;
use midly::TrackEventKind;
use std::env;
use std::process::exit;
use std::{collections::BTreeMap, fs};

fn main() {
    // Get the command-line arguments
    let args: Vec<String> = env::args().collect();
    let mut verbose = false; 

    if args.len() < 2 || args.len() > 4 {
        println!("Usage: midi2psx <input.mid> [output.dss] [--verbose]");
        exit(1)
    }

    if args[1].ends_with(".mid") == false {
        println!("Usage: midi2psx <input.mid> [output.dss]");
        exit(1)
    }

    if args.len() == 4 {
        if args[3] == "--verbose" {
            verbose = true;
        }
    }

    if verbose {
        env_logger::Builder::new().filter_level(log::LevelFilter::Debug).init();
    } else {
        env_logger::Builder::new().filter_level(log::LevelFilter::Info).init();
    }

    // Load MIDI file
    let bytes = match fs::read(
        &args[1],
    ) {
        Ok(x) => x,
        Err(_) => {error!("Failed to open file {}", args[1]); exit(2)},
    };
    let smf = Smf::parse(&bytes).unwrap();

    // Find output path
    let out_path;
    if args.len() < 3 {
        out_path = args[1].replace(".mid", ".dss");
    } else {
        out_path = args[2].clone();
    }

    // Read all the tracks and events, and squash them together into one track
    let mut event_map = BTreeMap::new();

    for (_, track) in smf.tracks.iter().enumerate() {
        let mut time = 0;
        for event in track {
            time += event.delta.as_int();
            event_map.entry(time).or_insert(Vec::new()).push(event.kind);
        }
    }

    // Now let's convert it into FlanSeqCommands
    let mut fdss_commands: Vec<FlanSeqCommand> = Vec::new();
    let mut prev_time = 0;
    let mut pitch_bend_range_coarse = 2;
    let mut pitch_bend_range_fine = 0;
    for (time, events) in event_map {
        if prev_time != time {
            let delta_time = time - prev_time;

            // Figure out what combination of ticks is necessary
            let mut delta_time_left = delta_time as u16;
            while delta_time_left > 0 {
            for index in (0..WAIT_TICK_LUT.len()).rev() {
                if WAIT_TICK_LUT[index] <= delta_time_left {
                    delta_time_left -= WAIT_TICK_LUT[index];
                    fdss_commands.push(FlanSeqCommand::WaitTicks { index_into_lut: index });
                    break;
                }
            }}
        }
        prev_time = time;
        let mut cc100 = -1;
        let mut cc101 = -1;
        for event in events {
            match event {
                TrackEventKind::Midi {channel, message} => {
                    match message {
                        midly::MidiMessage::NoteOn{key, vel} => fdss_commands.push(FlanSeqCommand::PlayNote { channel: channel.into(), key: key.into(), velocity: vel.into() }),
                        midly::MidiMessage::NoteOff{key, vel: _} => fdss_commands.push(FlanSeqCommand::ReleaseNote { channel: channel.into(), key: key.into() }),
                        midly::MidiMessage::ProgramChange{program} => {
                            let channel = u8::from(channel);
                            let index = match channel {
                                9 => u8::from(program) + 128,
                                _ => u8::from(program),
                            };
                            fdss_commands.push(FlanSeqCommand::SetChannelInstrument { channel: channel, index: index })
                        },
                        midly::MidiMessage::PitchBend {bend} => {
                            let pitch_bend_range_cents = (pitch_bend_range_coarse as f32 * 100.0) + (pitch_bend_range_fine as f32 * 1.0);
                            let bend_value_normalized = bend.as_f32();
                            let actual_bend_in_10th_of_cents = (pitch_bend_range_cents * 10.0) * bend_value_normalized;
                            fdss_commands.push(FlanSeqCommand::SetChannelPitch { channel: channel.into(), pitch: actual_bend_in_10th_of_cents as i16 })
                        },
                        midly::MidiMessage::Controller{controller, value} => match u8::from(controller) {
                            7 => fdss_commands.push(FlanSeqCommand::SetChannelVolume { channel: channel.into(), volume: value.into() }),
                            10 => fdss_commands.push(FlanSeqCommand::SetChannelPanning { channel: channel.into(), panning: u8::from(value) * 2 }),
                            100 => cc100 = u8::from(value) as i32,
                            101 => cc101 = u8::from(value) as i32,
                            6 => {
                                if cc100 == 0 && cc101 == 0 {
                                    pitch_bend_range_coarse = value.into()
                                }
                            }
                            38 => {
                                if cc100 == 0 && cc101 == 0 {
                                    pitch_bend_range_fine = value.into()
                                }
                            }
                            _ => debug!("Unsupported controller {controller}, value {value}"),
                        }
                        _ => debug!("Unsupported event {message:?}"),
                    }
                },
                TrackEventKind::Meta(message) => {
                    match message {
                        midly::MetaMessage::Tempo(tempo) => {
                            let ticks_per_quarter_note = match smf.header.timing {
                                midly::Timing::Metrical(ticks_per_quarter_note) => ticks_per_quarter_note.as_int() as f64,
                                midly::Timing::Timecode(..) => panic!("Attempted tempo change with fixed timecode for time division!")
                            };
                            let microseconds_per_quarter_note = tempo.as_int() as f64;
                            let microseconds_per_tick = microseconds_per_quarter_note / ticks_per_quarter_note;
                            let seconds_per_tick = microseconds_per_tick /  1_000_000.0;
                            let tick_length_multiplier = 49152.0;
                            let raw_value = (seconds_per_tick * tick_length_multiplier).round().clamp(0.0, 4095.0);
                            fdss_commands.push(FlanSeqCommand::SetTempo { tempo: raw_value as u16 })
                        },
                        midly::MetaMessage::TimeSignature(num, denom, _ticks_per_click, _note32_per_midi_quarter) => {
                            fdss_commands.push(FlanSeqCommand::SetTimeSignature { numerator: num, denominator: 1 << denom })
                        },
                        _ => debug!("Unsupported meta event {message:?}"),
                    }
                },
                _ => debug!("Unsupported event: {event:?}"),
            }
        }
    }

    // TODO: write header
    let mut output = Vec::<u8>::new();
    output.extend("FDSS".as_bytes());  // file magic
    output.extend(1u32.to_le_bytes()); // number of sections, currently forced to 1
    output.extend(0u32.to_le_bytes()); // section table offset, let's just define this to be the first thing after the header
    output.extend(4u32.to_le_bytes()); // section data offset, always 4 because number of sections is forced to 1
    output.extend(0u32.to_le_bytes()); // section table entry 1: starts at the start of the section data

    // Write sequence data to file
    for command in fdss_commands {
        output.extend(command.serialize());
    }

    if let Err(err) = fs::write(out_path, &output) {
        error!("Error writing to file: {}", err);
    } else {
        debug!("Data successfully written to file.");
    }
}

#[derive(Debug)]
pub enum FlanSeqCommand {
    // Channel commands
    ReleaseNote{channel: u8, key: u8},
    PlayNote{channel: u8, key: u8, velocity: u8},
    SetChannelVolume{channel: u8, volume: u8},
    SetChannelPanning{channel: u8, panning:u8},
    SetChannelPitch{channel: u8, pitch: i16},
    SetChannelInstrument{channel: u8, index: u8},

    // General commands
    SetTempo{tempo: u16},
    WaitTicks{index_into_lut: usize},
    SetTimeSignature{numerator: u8, denominator: u8},
    SetLoopStart,
    JumpToLoopStart,
}

impl FlanSeqCommand {
    fn serialize(self) -> Vec<u8> {
        match self {
            FlanSeqCommand::ReleaseNote         { channel, key } =>               return vec![0x00 | channel, key],
            FlanSeqCommand::PlayNote            { channel, key, velocity } => return vec![0x10 | channel, key, velocity],
            FlanSeqCommand::SetChannelVolume    { channel, volume } =>            return vec![0x20 | channel, volume],
            FlanSeqCommand::SetChannelPanning   { channel, panning } =>           return vec![0x30 | channel, panning],
            FlanSeqCommand::SetChannelPitch     { channel, pitch } => {
                let pitch_bytes = pitch.to_le_bytes();
                return vec![0x40 | channel, pitch_bytes[0], pitch_bytes[1]]
            },
            FlanSeqCommand::SetChannelInstrument { channel, index } =>            return vec![0x50 | channel, index],
            FlanSeqCommand::SetTempo            { tempo } =>                         return vec![0x80 | (tempo >> 8) as u8, (tempo & 0xFF) as u8],
            FlanSeqCommand::WaitTicks { index_into_lut } =>                        return vec![0xA0 + index_into_lut as u8],
            FlanSeqCommand::SetTimeSignature { numerator, denominator } =>        return vec![0xFD, numerator, denominator],
            FlanSeqCommand::SetLoopStart =>                                               return vec![0xFE],
            FlanSeqCommand::JumpToLoopStart =>                                            return vec![0xFF],
        }
    }
}

const WAIT_TICK_LUT: [u16; 32] = [
    1,      2,      3,      4,      6,      8,      12,     16,
    20,     24,     28,     32,     40,     48,     56,     64,
    80,     96,     112,    128,    160,    192,    224,    256,
    320,    384,    448,    512,    640,    768,    896,    1024,
];