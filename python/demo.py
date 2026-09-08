#!/usr/bin/env python3
"""Connect to a pair of E09 smart glasses over BLE and print decoded events.

    pip install bleak
    ./bindings/generate.sh python
    cp target/release/libluma_core.dylib bindings/out/python/   # .so on Linux
    PYTHONPATH=bindings/out/python python3 python/demo.py

    python3 python/demo.py                   # connect, handshake, listen for 20 s
    python3 python/demo.py --wifi gallery     # raise the SoftAP, print how to join, list files
    python3 python/demo.py --wifi live        # raise it for the RTSP live stream
    python3 python/demo.py --wifi close       # tear it down (0x44 30 00)

Everything protocol-shaped here comes from the generated bindings: the UUIDs from
`glasses_gatt()`, the outbound frames from the `glasses_*` command builders, the decode from
`GlassesParser`, the file-API URLs and parsing from `glasses_wifi_*`, and every duration from
`glasses_timing_*_ms()`. This file owns only the radio.

Not run against hardware in CI — there is none. Needs a real adapter and a real unit.
"""

import asyncio
import sys
import urllib.request

from bleak import BleakClient, BleakScanner

import luma_core as gp

GATT = gp.glasses_gatt()
SIG_BASE = "0000{}-0000-1000-8000-00805f9b34fb"


def uuid(short: str) -> str:
    """Expand a 16-bit short UUID against the SIG base, which is what bleak wants."""
    return SIG_BASE.format(short.lower())


SERVICE = uuid(GATT.service)
WRITE = uuid(GATT.write)
NOTIFY = [uuid(u) for u in GATT.notify_all]


async def main() -> int:
    wifi_mode = None
    if "--wifi" in sys.argv:
        i = sys.argv.index("--wifi")
        wifi_mode = sys.argv[i + 1] if len(sys.argv) > i + 1 else "gallery"

    print(f"scanning for service {GATT.service}…")
    device = await BleakScanner.find_device_by_filter(
        lambda d, ad: SERVICE in [s.lower() for s in ad.service_uuids],
        timeout=15.0,
    )
    if device is None:
        print("no glasses found. Wake the unit and make sure nothing else is connected.")
        return 1

    print(f"found {device.name or '(unnamed)'} @ {device.address}")

    # One parser for both notify characteristics: control replies and voice frames both
    # arrive as AC 55 envelopes, and the parser routes the 52 58 file stream out as a
    # Foreign residual rather than mis-decoding it.
    parser = gp.GlassesParser()
    seen: list = []

    def on_notify(char, data: bytearray) -> None:
        for event in parser.push(bytes(data)):
            seen.append(event)
            print(f"  {event}")

    async with BleakClient(device) as client:
        print(f"connected. subscribing to {', '.join(GATT.notify_all)}")
        for char in NOTIFY:
            await client.start_notify(char, on_notify)

        # AA13 takes an ATT Write REQUEST on this firmware, never a Write Command.
        # bleak spells that `response=True`; sending it the other way gets no reply and
        # no error either, which is the single easiest way to waste an afternoon here.
        assert GATT.write_with_response

        async def send(name: str, frame: bytes) -> None:
            print(f"-> {name}: {frame.hex(' ')}")
            await client.write_gatt_char(WRITE, frame, response=True)
            await asyncio.sleep(0.4)

        if wifi_mode:
            code = await run_wifi(send, seen, wifi_mode)
            for char in NOTIFY:
                await client.stop_notify(char)
            return code

        await send("get_versions", gp.glasses_get_versions())
        await send("get_project_name", gp.glasses_get_project_name())
        await send("get_battery", gp.glasses_get_battery())
        await send("get_capabilities", gp.glasses_get_capabilities())
        # The 0x48 read answers with a TEN-FRAME BURST keyed by each setting's own
        # setter opcode. There is no 0x48 reply; waiting for one waits forever. The
        # parser accumulates the burst for you — read it back below.
        await send("get_switch_states", gp.glasses_get_switch_states())

        print("\nlistening for 20 s (touch the pad, say the wake word, take a photo)…")
        await asyncio.sleep(20.0)

        states = parser.switch_states()
        print("\naccumulated settings:")
        print(f"  led            {states.led}")
        print(f"  record seconds {states.record_seconds}")
        print(f"  wear detection {states.wear_detection}")
        print(f"  voice command  {states.voice_command}")
        print(f"  orientation    {states.orientation}")
        for slot, action in zip(gp.glasses_gesture_slots(), states.gestures):
            print(f"  gesture {slot!s:<38} {action}")
        print(f"  complete       {states.is_complete}")

        # 0x56 is the ONLY thing that closes the mic on this firmware — 0x99 is declared
        # by the vendor spec and never sent. Write it unconditionally on the way out.
        await send("interrupt_voice", gp.glasses_interrupt_voice())

        for char in NOTIFY:
            await client.stop_notify(char)

    print("disconnected.")
    return 0


async def run_wifi(send, seen, mode: str) -> int:
    """Raise the glasses' access point, say how to join it, and list what is on it.

    The wait AFTER the SSID arrives is the part clients get wrong: the access point is not
    accepting associations at the moment the 0x25 push lands, and joining then fails as
    "incorrect password" — which iOS remembers. `glasses_timing_ssid_settle_ms()` is that
    wait, and this sleeps it before saying anything about joining.
    """
    if mode == "close":
        # 0x44 30 00 — all done. Write this before leaving the network, or the phone sits on
        # an access point with no route out.
        await send("file_download_complete", gp.glasses_file_download_complete())
        print("access point torn down. Leave the network on this machine too.")
        return 0

    service = (
        gp.FfiGlassesWifiService.LIVE if mode == "live" else gp.FfiGlassesWifiService.FILES
    )
    await send(f"open_wifi({mode})", gp.glasses_open_wifi(service, False))

    # The SSID arrives on 0x25, about glasses_timing_ssid_arrival_ms() after the ack.
    budget = gp.glasses_timing_ssid_arrival_ms() * 6 / 1000.0
    print(f"waiting up to {budget:.0f}s for the SSID (0x25)…")
    ssid = None
    waited = 0.0
    while waited < budget and ssid is None:
        await asyncio.sleep(0.25)
        waited += 0.25
        for e in seen:
            if isinstance(e, gp.FfiGlassesEvent.WIFI_CREDENTIALS):
                ssid = e.ssid
                break
    if ssid is None:
        print("no SSID arrived — the glasses did not raise the access point.")
        return 1

    settle = gp.glasses_timing_ssid_settle_ms() / 1000.0
    passphrase = gp.glasses_wifi_passphrase()
    print(f"\n  SSID        {ssid}")
    print(f"  passphrase  {passphrase}")
    print(f"  glasses at  {gp.glasses_wifi_host()}   (you get {gp.glasses_wifi_phone_address()})")
    print(f"\nwaiting {settle:.0f}s — the AP is not ready the moment the SSID arrives…")
    await asyncio.sleep(settle)

    print("\njoin it:")
    print(f"  macOS:  networksetup -setairportnetwork en0 {ssid} {passphrase}")
    print(f"  Linux:  nmcli dev wifi connect {ssid} password {passphrase}")

    if mode == "live":
        print(f"\nthen point a player at {gp.glasses_rtsp_url()}, or run:")
        print("  cargo run --example live -- 10")
        print("\nleaving the BLE link up. Press Enter when you are done to tear the AP down.")
        await asyncio.to_thread(input)
        await send("file_download_complete", gp.glasses_file_download_complete())
        return 0

    print("\nPress Enter once you are on that network and this will list the files.")
    await asyncio.to_thread(input)

    try:
        body = urllib.request.urlopen(gp.glasses_wifi_list_url(), timeout=30).read()
    except Exception as e:
        print(f"\n{gp.glasses_wifi_host()} did not answer: {e}")
        print("Still on the old network? Check the Wi-Fi menu and try again.")
        return 1

    result = gp.glasses_wifi_parse_file_list(body.decode("utf-8", "replace"))
    if not isinstance(result, gp.FfiGlassesFileListResult.OK):
        print(f"the listing did not parse: {result.reason}")
        return 1

    listing = result.list
    for folder in listing.folders:
        print(f"\n{folder.folder.name} — {len(folder.files)} file(s)")
        for f in folder.files:
            # `size` is KiB and floored, so a complete download is a RANGE of byte counts.
            print(f"  {f.basename:<28} {f.size_kib:>6} KiB "
                  f"({f.min_bytes}..{f.max_bytes} bytes)")
    print(f"\n{listing.total_files} file(s), {listing.total_size_kib} KiB.")
    print("\nDownload them with:  python3 python/gallery.py sync ./gallery")

    # The BLE link is still up, so tear the AP down here rather than making the user reconnect.
    print("\nPress Enter to tear the access point down now.")
    await asyncio.to_thread(input)
    await send("file_download_complete", gp.glasses_file_download_complete())
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        sys.exit(130)
