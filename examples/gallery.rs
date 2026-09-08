//! Photos, voice notes and clips off the glasses over their own Wi-Fi. Feature `wifi-client`.
//!
//! ```text
//! cargo run --example gallery --features wifi-client -- list
//! cargo run --example gallery --features wifi-client -- download EVENT/20260727223716.jpg [dir]
//! cargo run --example gallery --features wifi-client -- download --all [dir]
//! cargo run --example gallery --features wifi-client -- delete EVENT/20260727223716.jpg
//! cargo run --example gallery --features wifi-client -- sync <dir> [--keep]
//! cargo run --example gallery --features wifi-client -- plan
//! ```
//!
//! The glasses raise their own access point on demand and serve the file API on it (§12–§13).
//! Getting on that network is the part this example cannot do for you — it is an OS-level
//! operation and, on macOS, one that needs the Wi-Fi menu or `networksetup`. When the host does
//! not answer, this prints the exact commands.
//!
//! `sync` downloads every file and then deletes it from the glasses, one at a time, verifying
//! each download against the listing's `size` before the delete goes out. `--keep` skips the
//! deletes.
//!
//! Not run against hardware from the environment this was written in: there is no access point
//! and no unit here. The parsing and the completeness rule underneath it are pinned by
//! `fileapi`'s tests against a real listing and four real downloads.

use std::path::{Path, PathBuf};

use luma_core::client::fileapi::FileClient;
use luma_core::fileapi::{self, FileList, SyncStep};

const USAGE: &str = "\
usage: gallery <command>

  list                        every file on the glasses, by folder
  download <name> [dir]       one file, verified against its listed size
  download --all [dir]        every file
  delete <name>               remove one file from the glasses
  sync <dir> [--keep]         download everything, then delete it unless --keep
  plan                        print the recommended sync order without doing it

<name> is the listing's own name, folder prefix included: EVENT/20260727223716.jpg
[dir] defaults to ./gallery
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print!("{USAGE}");
        std::process::exit(2);
    }

    let client = FileClient::new();
    let result = match args[0].as_str() {
        "list" => cmd_list(&client),
        "download" => cmd_download(&client, &args[1..]),
        "delete" => cmd_delete(&client, &args[1..]),
        "sync" => cmd_sync(&client, &args[1..]),
        "plan" => cmd_plan(&client),
        other => {
            eprintln!("unknown command `{other}`\n");
            print!("{USAGE}");
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("\n{e}");
        if !client.is_reachable() {
            print_join_instructions();
        }
        std::process::exit(1);
    }
}

fn print_join_instructions() {
    println!(
        "\n{} is not answering, so this machine is not on the glasses' network.\n",
        fileapi::HOST
    );
    println!("  1. Bring the access point up over BLE — write 0x39:");
    println!("       cargo run --example luma --features ble -- wifi gallery");
    println!("     (or run python/demo.py --wifi gallery)");
    println!("  2. It prints an SSID like DH-TwI-4641E4. Wait ~2 s, then join it:");
    println!(
        "       macOS:  networksetup -setairportnetwork en0 <SSID> {}",
        fileapi::PASSPHRASE
    );
    println!(
        "       Linux:  nmcli dev wifi connect <SSID> password {}",
        fileapi::PASSPHRASE
    );
    println!("     The passphrase is fixed on every unit.");
    println!(
        "  3. Run this again. The glasses are {}; you are {}.",
        fileapi::HOST,
        fileapi::PHONE_ADDRESS
    );
    println!(
        "\n  On iOS and macOS, turning cellular/other interfaces off for the duration helps —"
    );
    println!("  the AP has no route to the internet and the OS would rather leave it.");
}

fn cmd_list(client: &FileClient) -> Result<(), String> {
    let list = client.list().map_err(|e| e.to_string())?;
    print_list(&list);
    Ok(())
}

fn print_list(list: &FileList) {
    if list.is_empty() {
        println!("the glasses hold no files.");
        return;
    }
    for folder in &list.folders {
        println!(
            "\n{} ({:?}) — {} file(s){}",
            folder.folder.name(),
            folder.folder.holds(),
            folder.files.len(),
            if folder.count_matches_rows() {
                String::new()
            } else {
                // The device's own count disagreeing with its rows is a real observation, not a
                // parse failure — see FolderListing::count.
                format!(", device count says {}", folder.count)
            }
        );
        for f in &folder.files {
            let range = f.expected_byte_range();
            println!(
                "  {:<28}  {:>6} KiB ({}..={} bytes)  {}",
                f.basename(),
                f.size_kib,
                range.start(),
                range.end(),
                f.created
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "no timestamp".into())
            );
        }
    }
    println!(
        "\n{} file(s), {} KiB total.",
        list.total_files(),
        list.total_size_kib()
    );
}

fn cmd_download(client: &FileClient, args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("download needs a name, or --all".into());
    }
    let all = args[0] == "--all";
    let dir = PathBuf::from(
        args.get(if all { 1 } else { 1 })
            .cloned()
            .unwrap_or_else(|| "gallery".into()),
    );

    let list = client.list().map_err(|e| e.to_string())?;
    let wanted: Vec<_> = if all {
        list.all_files().collect()
    } else {
        vec![list
            .find(&args[0])
            .ok_or_else(|| format!("no file called `{}` — run `gallery list`", args[0]))?]
    };

    for entry in wanted {
        print!("{} … ", entry.name);
        use std::io::Write;
        std::io::stdout().flush().ok();
        match client.download_to_dir(entry, &dir) {
            Ok(path) => {
                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                println!("{bytes} bytes → {}", path.display());
            }
            Err(e) => println!("FAILED: {e}"),
        }
    }
    Ok(())
}

fn cmd_delete(client: &FileClient, args: &[String]) -> Result<(), String> {
    let name = args.first().ok_or("delete needs a name")?;
    let reply = client.delete(name).map_err(|e| e.to_string())?;
    println!("{name}: {} ({})", reply.info, reply.result);
    Ok(())
}

fn cmd_sync(client: &FileClient, args: &[String]) -> Result<(), String> {
    let dir = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("gallery"));
    let keep = args.iter().any(|a| a == "--keep");

    println!(
        "syncing into {} ({})",
        dir.display(),
        if keep {
            "keeping the originals"
        } else {
            "deleting each file once it has landed"
        }
    );
    let report = client
        .sync_all(Path::new(&dir), !keep)
        .map_err(|e| e.to_string())?;

    println!(
        "\n{} file(s), {} bytes.",
        report.downloaded.len(),
        report.bytes
    );
    if !keep {
        println!("{} deleted from the glasses.", report.deleted.len());
    }
    for (name, why) in &report.failures {
        println!("  FAILED {name}: {why}");
    }
    println!("\nNow write 0x44 30 00 over BLE and leave the network:");
    println!("  cargo run --example luma --features ble -- wifi close");
    if report.is_clean() {
        Ok(())
    } else {
        Err(format!("{} file(s) did not sync", report.failures.len()))
    }
}

fn cmd_plan(client: &FileClient) -> Result<(), String> {
    let plan = client.plan(true).map_err(|e| e.to_string())?;
    println!("{} step(s):", plan.len());
    for step in plan.iter() {
        match step {
            SyncStep::List => println!("  GET {}", fileapi::list_url()),
            SyncStep::Thumbnail { name } => println!("  GET {}", fileapi::thumbnail_url(name)),
            SyncStep::Download { name, size_kib } => {
                let r = fileapi::expected_byte_range(*size_kib);
                println!(
                    "  GET {}   (expect {}..={} bytes)",
                    fileapi::download_url(name),
                    r.start(),
                    r.end()
                );
            }
            SyncStep::Delete { name } => println!("  GET {}", fileapi::delete_url(name)),
            SyncStep::TearDown => println!("  BLE 0x44 30 00, then leave the network"),
        }
    }
    println!(
        "\nat most {} bytes of downloads.",
        plan.max_download_bytes()
    );
    Ok(())
}
