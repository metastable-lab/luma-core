#!/usr/bin/env python3
"""Pull photos, voice notes and clips off the glasses over their own Wi-Fi.

    ./bindings/generate.sh python
    cp target/release/libluma_core.dylib bindings/out/python/   # .so on Linux
    PYTHONPATH=bindings/out/python python3 python/gallery.py list

    python3 python/gallery.py list
    python3 python/gallery.py download EVENT/20260727223716.jpg [dir]
    python3 python/gallery.py download --all [dir]
    python3 python/gallery.py delete EVENT/20260727223716.jpg
    python3 python/gallery.py sync <dir> [--keep]

urllib only — no requests, no aiohttp. Everything protocol-shaped comes from the generated
`luma_core` bindings: the host, the four URLs, the JSON parsing and the KiB completeness rule.
This file owns the socket and the CLI.

Before it can talk to anything, this machine has to be on the glasses' own access point:
raise it with 0x39 over BLE (`python3 python/demo.py --wifi gallery`), join the SSID it
prints, then run this. The script says so when the host does not answer.

Not run against hardware in CI — there is none. Needs a real unit and a real join.
"""

import os
import sys
import urllib.error
import urllib.request

import luma_core as luma

# A generous timeout: a 400 KB photo over a SoftAP takes a few seconds, and the AP has no
# route to the internet so nothing else is competing for it.
TIMEOUT = 60.0

USAGE = __doc__.split("urllib only")[0].strip()


def get(url: str) -> bytes:
    """One GET, with the body read whole."""
    with urllib.request.urlopen(url, timeout=TIMEOUT) as r:
        if r.status != 200:
            raise RuntimeError(f"{url}: HTTP {r.status}")
        return r.read()


def reachable() -> bool:
    try:
        get(luma.glasses_wifi_list_url())
        return True
    except Exception:
        return False


def join_instructions() -> None:
    host = luma.glasses_wifi_host()
    passphrase = luma.glasses_wifi_passphrase()
    print(f"\n{host} is not answering, so this machine is not on the glasses' network.\n")
    print("  1. Bring the access point up over BLE (writes 0x39):")
    print("       python3 python/demo.py --wifi gallery")
    print("  2. It prints an SSID like DH-TwI-4641E4. Wait ~2 s, then join it:")
    print(f"       macOS:  networksetup -setairportnetwork en0 <SSID> {passphrase}")
    print(f"       Linux:  nmcli dev wifi connect <SSID> password {passphrase}")
    print("     The passphrase is fixed on every unit.")
    print(f"  3. Run this again. The glasses are {host}; you get "
          f"{luma.glasses_wifi_phone_address()}.")


def fetch_list():
    """GET /app/getfilelist, parsed by the crate."""
    body = get(luma.glasses_wifi_list_url()).decode("utf-8", "replace")
    result = luma.glasses_wifi_parse_file_list(body)
    if not isinstance(result, luma.FfiGlassesFileListResult.OK):
        raise RuntimeError(f"the listing did not parse: {result.reason}")
    return result.list


def all_files(listing):
    for folder in listing.folders:
        for f in folder.files:
            yield f


def cmd_list() -> int:
    listing = fetch_list()
    if listing.total_files == 0:
        print("the glasses hold no files.")
        return 0
    for folder in listing.folders:
        note = ""
        if folder.count != len(folder.files):
            # The device's own count disagreeing with its rows is a real observation.
            note = f", device count says {folder.count}"
        print(f"\n{folder.folder.name} — {len(folder.files)} file(s){note}")
        for f in folder.files:
            when = "no timestamp"
            if f.created is not None:
                c = f.created
                when = f"{c.year:04}-{c.month:02}-{c.day:02} {c.hour:02}:{c.minute:02}:{c.second:02}"
            print(f"  {f.basename:<28} {f.size_kib:>6} KiB "
                  f"({f.min_bytes}..{f.max_bytes} bytes)  {when}")
    print(f"\n{listing.total_files} file(s), {listing.total_size_kib} KiB total.")
    return 0


def download_one(entry, directory: str) -> str:
    """GET the file and verify its length before anything touches the disk.

    `size` in the listing is KIBIBYTES and floored, so the check is a RANGE. Comparing the body
    length to `size` rejects every download; demanding `size * 1024` rejects 1023 in 1024.
    """
    body = get(luma.glasses_wifi_download_url(entry.name))
    if not luma.glasses_wifi_download_is_complete(entry.size_kib, len(body)):
        raise RuntimeError(
            f"{entry.name}: got {len(body)} bytes, a {entry.size_kib} KiB listing means "
            f"{entry.min_bytes}..{entry.max_bytes}"
        )
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, entry.basename)
    with open(path, "wb") as fh:
        fh.write(body)
    return path


def cmd_download(args) -> int:
    if not args:
        print("download needs a name, or --all")
        return 2
    directory = args[1] if len(args) > 1 else "gallery"
    listing = fetch_list()
    if args[0] == "--all":
        wanted = list(all_files(listing))
    else:
        wanted = [f for f in all_files(listing) if f.name == args[0]]
        if not wanted:
            print(f"no file called `{args[0]}` — run `gallery.py list`")
            return 1
    for entry in wanted:
        try:
            path = download_one(entry, directory)
            print(f"{entry.name} -> {path} ({os.path.getsize(path)} bytes)")
        except Exception as e:  # one bad file must not cost the rest
            print(f"{entry.name}: FAILED: {e}")
    return 0


def cmd_delete(args) -> int:
    if not args:
        print("delete needs a name")
        return 2
    body = get(luma.glasses_wifi_delete_url(args[0])).decode("utf-8", "replace")
    reply = luma.glasses_wifi_parse_delete_reply(body)
    if not isinstance(reply, luma.FfiGlassesDeleteReplyResult.OK):
        print(f"the reply did not parse: {reply.reason}")
        return 1
    print(f"{args[0]}: {reply.reply.info} ({reply.reply.result})")
    return 0 if reply.reply.success else 1


def cmd_sync(args) -> int:
    directory = next((a for a in args if not a.startswith("--")), "gallery")
    keep = "--keep" in args
    listing = fetch_list()
    failures = 0
    for entry in all_files(listing):
        try:
            path = download_one(entry, directory)
            print(f"{entry.name} -> {path}")
        except Exception as e:
            print(f"{entry.name}: FAILED: {e}")
            failures += 1
            continue  # never delete what did not land
        if not keep:
            body = get(luma.glasses_wifi_delete_url(entry.name)).decode("utf-8", "replace")
            reply = luma.glasses_wifi_parse_delete_reply(body)
            ok = isinstance(reply, luma.FfiGlassesDeleteReplyResult.OK) and reply.reply.success
            print(f"  deleted from the glasses: {ok}")
    print("\nNow write 0x44 30 00 over BLE and leave the network:")
    print("  python3 python/demo.py --wifi close")
    return 1 if failures else 0


def main() -> int:
    args = sys.argv[1:]
    if not args:
        print(USAGE)
        return 2
    command, rest = args[0], args[1:]
    handlers = {
        "list": lambda: cmd_list(),
        "download": lambda: cmd_download(rest),
        "delete": lambda: cmd_delete(rest),
        "sync": lambda: cmd_sync(rest),
    }
    if command not in handlers:
        print(f"unknown command `{command}`\n")
        print(USAGE)
        return 2
    try:
        return handlers[command]()
    except (urllib.error.URLError, OSError, RuntimeError) as e:
        print(f"\n{e}")
        if not reachable():
            join_instructions()
        return 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
