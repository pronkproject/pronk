use drm_executor_replay::{render, MAX_FIXTURE_BYTES};
use serde_json::{json, Value};

const OPAQUE: &[u8] = include_bytes!("../fixtures/opaque-crop.json");
const ALPHA: &[u8] = include_bytes!("../fixtures/alpha-stack.json");
const TRANSFORM: &[u8] = include_bytes!("../fixtures/reflected-quarter-turn.json");

#[test]
fn saved_scenes_match_literal_rgb_goldens() {
    fn check(scene: &[u8], header: &[u8], pixels: &[[u8; 3]]) {
        let mut output = Vec::new();
        render(scene).unwrap().write_ppm(&mut output).unwrap();
        let mut expected = header.to_vec();
        expected.extend(pixels.iter().flatten());
        assert_eq!(output, expected);
    }
    check(
        OPAQUE,
        b"P6\n3 2\n255\n",
        &[[0, 255, 0], [0; 3], [0; 3], [255; 3], [0; 3], [0; 3]],
    );
    check(ALPHA, b"P6\n2 1\n255\n", &[[128, 0, 127], [64, 48, 143]]);
    check(
        TRANSFORM,
        b"P6\n2 3\n255\n",
        &[
            [255, 0, 0],
            [255, 255, 0],
            [0, 255, 0],
            [0, 255, 255],
            [0, 0, 255],
            [255, 0, 255],
        ],
    );
}

fn reject(edit: impl FnOnce(&mut Value)) {
    let mut fixture: Value = serde_json::from_slice(OPAQUE).unwrap();
    edit(&mut fixture);
    assert!(render(&serde_json::to_vec(&fixture).unwrap()).is_err());
}

#[test]
fn invalid_fixture_contracts_fail_before_output() {
    reject(|v| v["version"] = json!(2));
    reject(|v| v["unknown"] = json!(true));
    reject(|v| v["output"]["unknown"] = json!(true));
    reject(|v| v["sources"][0]["unknown"] = json!(true));
    reject(|v| v["layers"][0]["unknown"] = json!(true));
    reject(|v| v["layers"][0]["source"] = json!(99));
    reject(|v| v["layers"][0]["crop_16_16"][0] = json!(1));
    reject(|v| v["layers"][0]["rotation"] = json!(45));
    reject(|v| v["layers"][0]["plane_alpha"] = json!(65536));
    reject(|v| v["sources"][0]["stride"] = json!(7));
    reject(|v| v["sources"][0]["bytes"] = json!([0, 0, 0, 0]));
    reject(|v| v["sources"][0]["format"] = json!("NV12"));
    reject(|v| v["output"]["width"] = json!(0));
    reject(|v| v["output"]["width"] = json!(u32::MAX));
    reject(|v| {
        v["sources"] = Value::Array(vec![v["sources"][0].clone(); 65]);
    });
    reject(|v| {
        v["layers"] = Value::Array(vec![v["layers"][0].clone(); 257]);
    });
    assert!(render(&vec![b' '; MAX_FIXTURE_BYTES + 1]).is_err());
}

#[test]
fn image_writer_preserves_io_failure() {
    struct Failed;
    impl std::io::Write for Failed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert_eq!(
        render(OPAQUE)
            .unwrap()
            .write_ppm(&mut Failed)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::BrokenPipe,
    );
}
