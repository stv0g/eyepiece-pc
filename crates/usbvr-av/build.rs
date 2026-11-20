// Generates FFmpeg bindings with bindgen against the installed headers.
//
// Normal case: FFmpeg dev packages installed (Debian: libavcodec-dev libavfilter-dev
// libavformat-dev libavutil-dev libswscale-dev) → found with pkg-config.
// Without them: FFMPEG_INCLUDE_DIR=<dir with libavcodec/…> and FFMPEG_LIB_DIR=<dir with
// libavcodec.so …> (e.g. headers unpacked from the .debs, see README).
use std::env;
use std::path::PathBuf;

const LIBS: &[(&str, &str)] = &[
    ("libavcodec", "avcodec"),
    ("libavfilter", "avfilter"),
    ("libavformat", "avformat"),
    ("libavutil", "avutil"),
];

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=FFMPEG_LIB_DIR");
    println!("cargo:rerun-if-changed=wrapper.h");
    let mut includes: Vec<PathBuf> = vec![];
    if let Ok(dir) = env::var("FFMPEG_INCLUDE_DIR") {
        includes.push(dir.into());
        if let Ok(l) = env::var("FFMPEG_LIB_DIR") {
            println!("cargo:rustc-link-search=native={l}");
        }
        for (_, l) in LIBS {
            println!("cargo:rustc-link-lib=dylib={l}");
        }
    } else {
        for (pc, _) in LIBS {
            let lib = pkg_config::Config::new().probe(pc).unwrap_or_else(|e| {
                panic!(
                    "{pc} not found ({e}).\nInstall the FFmpeg development packages \
                     (Debian/Ubuntu: sudo apt install libavcodec-dev libavfilter-dev libavformat-dev libavutil-dev) \
                     or set FFMPEG_INCLUDE_DIR / FFMPEG_LIB_DIR."
                )
            });
            includes.extend(lib.include_paths);
        }
    }
    let mut b = bindgen::Builder::default()
        .header("wrapper.h")
        .allowlist_function("(av|avcodec|avfilter|avformat|avio)_.*")
        .allowlist_type("(AV|av).*")
        .allowlist_var("(AV|FF)_.*")
        .allowlist_var("(AVERROR|AVSEEK)_.*")
        .default_enum_style(bindgen::EnumVariation::Rust { non_exhaustive: false })
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for i in &includes {
        b = b.clang_arg(format!("-I{}", i.display()));
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ffi.rs");
    b.generate().expect("bindgen failed (is libclang installed? set LIBCLANG_PATH)").write_to_file(out).unwrap();
}
