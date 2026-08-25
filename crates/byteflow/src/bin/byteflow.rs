use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use byteflow::samples::{self, add_forty_two, ping_pong};
use byteflow::{
    decode, disassemble, encode, std_native_table, verify, NativeTable, ProcessOutcome, Runtime,
    RuntimeConfig,
};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let cmd = match args.next() {
        Some(c) => c,
        None => "help".into(),
    };
    let result = match cmd.as_str() {
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "verify" => match args.next() {
            Some(path) => cmd_verify(&path),
            None => usage("byteflow verify <file.bf>"),
        },
        "disasm" => match args.next() {
            Some(path) => cmd_disasm(&path),
            None => usage("byteflow disasm <file.bf>"),
        },
        "run" => match args.next() {
            Some(path) => cmd_run(&path, args.next().as_deref()),
            None => usage("byteflow run <file.bf> [function]"),
        },
        "pack" => match (args.next(), args.next()) {
            (Some(demo), Some(out)) => cmd_pack(&demo, &out),
            _ => usage("byteflow pack <demo> <out.bf>"),
        },
        "demo" => {
            let demo = match args.next() {
                Some(d) => d,
                None => "ping-pong".into(),
            };
            cmd_demo(&demo)
        }
        other => {
            eprintln!("unknown command {other:?}");
            print_help();
            Err(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

fn usage(msg: &str) -> Result<(), ()> {
    eprintln!("usage: {msg}");
    Err(())
}

fn print_help() {
    eprintln!(
        "\
byteflow — verify, disassemble and run .bf modules (assembled via ChunkBuilder)

USAGE:
    byteflow demo [ping-pong|add]
    byteflow pack  <ping-pong|add> <out.bf>
    byteflow verify <file.bf>
    byteflow disasm <file.bf>
    byteflow run    <file.bf> [function]

`run` attaches the std native table (print=0, now_ms=1, make_msg=2, …) so modules
that CallNative those indices work. Demos that never call natives use an empty table.
"
    );
}

fn load_bf(path: &str) -> Result<byteflow::Chunk, ()> {
    let bytes = fs::read(path).map_err(|e| {
        eprintln!("read {path}: {e}");
    })?;
    decode(&bytes).map_err(|e| {
        eprintln!("{path}: {e}");
    })
}

fn cmd_verify(path: &str) -> Result<(), ()> {
    let chunk = load_bf(path)?;
    match verify(&chunk) {
        Ok(()) => {
            println!(
                "ok  {}  {} functions  {} instructions",
                chunk.name,
                chunk.functions.len(),
                chunk.code.len()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("verify failed: {e}");
            Err(())
        }
    }
}

fn cmd_disasm(path: &str) -> Result<(), ()> {
    let chunk = load_bf(path)?;
    print!("{}", disassemble(&chunk));
    Ok(())
}

fn cmd_run(path: &str, function: Option<&str>) -> Result<(), ()> {
    let chunk = load_bf(path)?;
    run_chunk(&chunk, function, std_native_table())
}

fn cmd_pack(demo: &str, out: &str) -> Result<(), ()> {
    let chunk = demo_chunk(demo)?;
    let bytes = encode(&chunk);
    if let Some(dir) = Path::new(out).parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(|e| eprintln!("mkdir: {e}"))?;
        }
    }
    fs::write(out, bytes).map_err(|e| eprintln!("write {out}: {e}"))?;
    println!(
        "wrote {out} ({} bytes, chunk {:?})",
        fs::metadata(out).map(|m| m.len()).unwrap_or(0),
        chunk.name
    );
    Ok(())
}

fn cmd_demo(name: &str) -> Result<(), ()> {
    let chunk = demo_chunk(name)?;
    run_chunk(&chunk, Some("main"), NativeTable::empty())
}

fn demo_chunk(name: &str) -> Result<byteflow::Chunk, ()> {
    match name {
        "ping-pong" | "ping_pong" => Ok(ping_pong()),
        "add" | "add-forty-two" | "42" => Ok(add_forty_two()),
        "boom" => Ok(samples::boom()),
        other => {
            eprintln!("unknown demo {other:?} (try ping-pong, add)");
            Err(())
        }
    }
}

fn run_chunk(
    chunk: &byteflow::Chunk,
    function: Option<&str>,
    natives: Arc<NativeTable>,
) -> Result<(), ()> {
    if let Err(e) = verify(chunk) {
        eprintln!("verify failed: {e}");
        return Err(());
    }

    let rt = Runtime::with_natives_and_config(
        chunk.clone(),
        natives,
        RuntimeConfig {
            workers: 1,
            quantum: byteflow::DEFAULT_QUANTUM,
        },
    )
    .map_err(|e| eprintln!("runtime: {e}"))?;

    let idx = match function {
        Some(name) => rt.function_index(name).ok_or_else(|| {
            eprintln!("no function named {name:?}");
        })?,
        None => rt
            .function_index("main")
            .or_else(|| (!chunk.functions.is_empty()).then_some(0))
            .ok_or_else(|| eprintln!("chunk has no functions"))?,
    };

    let outcome = rt
        .spawn(idx, &[])
        .map_err(|e| eprintln!("spawn: {e}"))?
        .join();
    let metrics = rt.metrics();
    rt.shutdown();

    match outcome {
        ProcessOutcome::Completed(v) => println!("{v}"),
        ProcessOutcome::Failed(err) => {
            eprintln!("process failed: {err}");
            eprintln!("{metrics}");
            return Err(());
        }
    }
    eprintln!("{metrics}");
    Ok(())
}
