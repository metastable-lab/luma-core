#!/usr/bin/env python3
"""Connect to a pair of E09 smart glasses over BLE and print decoded events.

    pip install bleak
    ./bindings/generate.sh python
    cp target/release/libluma_core.dylib bindings/out/python/   # .so on Linux
    PYTHONPATH=bindings/out/python python3 python/demo.py

Everything protocol-shaped here comes from the generated bindings: the UUIDs from
`glasses_gatt()`, the outbound frames from the `glasses_*` command builders, and the
decode from `GlassesParser`. This file owns only the radio.

Not run against hardware in CI — there is none. Needs a real adapter and a real unit.
"""

import asyncio
import sys

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

    def on_notify(char, data: bytearray) -> None:
        for event in parser.push(bytes(data)):
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


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        sys.exit(130)
