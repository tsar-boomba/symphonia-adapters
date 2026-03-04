use std::fs::File;

use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::formats::TrackType;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::{FromStd, MediaSourceStream};
use symphonia::default::get_probe;
use symphonia_adapter_fdk_aac::AacDecoder;
use symphonia_adapter_libopus::OpusDecoder;

#[futures_test::test]
async fn test_decode_aac() {
    test_decode(File::open("../assets/music.m4a").unwrap()).await;
}

#[futures_test::test]
async fn test_decode_opus() {
    test_decode(File::open("../assets/sample.opus").unwrap()).await;
}

async fn test_decode(file: File) {
    let mss = MediaSourceStream::new(Box::new(FromStd::new(file)), Default::default());
    let mut reader = get_probe()
        .probe(&Hint::new(), mss, Default::default(), Default::default())
        .await
        .unwrap();
    let mut registry = CodecRegistry::new();
    registry.register_audio_decoder::<AacDecoder>();
    registry.register_audio_decoder::<OpusDecoder>();

    let track = reader.default_track(TrackType::Audio).unwrap();
    let track_id = track.id;
    let mut decoder = registry
        .make_audio_decoder(
            track.codec_params.as_ref().unwrap().audio().unwrap(),
            &Default::default(),
        )
        .await
        .unwrap();

    loop {
        let packet_res = reader.next_packet().await;

        let Some(packet) = packet_res.unwrap() else {
            break;
        };

        if packet.track_id() != track_id {
            continue;
        }

        decoder.decode(&packet).await.map(|_| ()).unwrap();
    }
}
