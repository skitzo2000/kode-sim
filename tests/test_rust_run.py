"""`kode-sim run`, from outside: the binary, the fake esp-emu, a page.

The page's WebSocket, the console both ways, the serial port, reset,
power, the battery, the microSD, the Origin check, --forward, crashes and
api/install - plus the board's chips, which kode-sim answers in-process.
"""

import asyncio
import json
import os
import re
import shutil
import signal
import struct
import subprocess
import time
import zlib

import aiohttp
import pytest

import kd
from sim import Sim, _closed, card, close, next_frame, next_json, open_serial, serial_read_until, serial_write, start, ws
async def _console_says(w, text, timeout=5):
    return await next_json(w, lambda j: j.get("console") == text, timeout)


async def _console_starts(w, text, timeout=5):
    return await next_json(w, lambda j: str(j.get("console", "")).startswith(text), timeout)


async def _past_history(w):
    """Past the console a page is sent on connecting: what follows is new."""
    await w.send_str(json.dumps({"console_in": "now"}))
    await _console_says(w, "echo: now")


# ---- the page ------------------------------------------------------------------

async def test_the_page_is_served(sim):
    async with aiohttp.ClientSession() as s:
        for path, kind in (("", "text/html"), ("sim.js", "javascript"), ("audio-worklet.js", "javascript"),
                           ("kode-dot.png", "image/png")):
            r = await s.get(sim.url + path)
            assert r.status == 200, path
            assert kind in r.headers["Content-Type"], (path, r.headers["Content-Type"])
        assert "sim.js" in await (await s.get(sim.url)).text()


async def test_new_client_gets_shadow_frame(sim):
    a = await ws(sim)
    await next_frame(a, kd.FB_RECT)              # live, as the fake paints
    b = await ws(sim)
    p = await next_frame(b, kd.FB_RECT)          # the shadow, whole
    x, y, w, h = struct.unpack_from("<HHHH", p)
    assert (x, y, w, h) == (0, 0, 410, 502)
    assert p[8:10] == b"\xff\xff" and p[8 + 2 * 2:8 + 2 * 3] == b"\x00\x00"
    await close(a)
    await close(b)


async def test_new_client_gets_battery_state_and_the_slot(sim):
    a = await ws(sim)
    seen = {}
    await next_json(a, lambda j: seen.update(j) or {"battery", "state", "sd"} <= seen.keys())
    assert seen["battery"] == {"percent": 80, "usb": True}
    assert seen["state"] == "running"
    assert seen["sd"] is True
    await close(a)


async def test_new_client_gets_recent_console(sim):
    a = await ws(sim)
    await next_json(a, lambda j: j.get("console") == "fake: up")
    await close(a)
    b = await ws(sim)                            # after the line went out
    await next_json(b, lambda j: j.get("console") == "fake: up")
    await close(b)


async def test_no_page_frame_reaches_kemu(sim):
    """The page's frames are the board's chips' and its codec's, which
    kode-sim answers itself: none goes to the firmware over UART1."""
    a = await ws(sim)
    await a.send_bytes(kd.encode(kd.PCM_IN, bytes(64)))
    await a.send_bytes(kd.encode(kd.BUTTONS, b"\x00"))
    await a.send_bytes(kd.encode(kd.TOUCH, struct.pack("<BHH", 0, 1, 1)))
    await a.send_str(json.dumps({"console_in": "hello"}))
    lines = []
    await next_json(a, lambda j: lines.append(j.get("console", "")) or j.get("console") == "echo: hello")
    assert not [l for l in lines if l.startswith(("rx 128 ", "rx 129 ", "rx 130 "))], lines
    await close(a)


async def test_only_the_page_s_own_frames_reach_the_board(sim):
    """A frame that is not the page's to send (LED) is dropped, as is a
    microphone frame bigger than a page sends: neither reaches the board."""
    a = await ws(sim)
    b = await ws(sim)
    await a.send_str(json.dumps({"mic": True}))
    await next_frame(a, kd.MIC_START)
    await a.send_str(json.dumps({"console_in": "rxformat 16000 2 16"}))
    await next_frame(a, kd.MIC_START)
    await a.send_bytes(kd.encode(kd.LED, b"\x01\x02\x03"))              # not the page's to send
    await a.send_bytes(kd.encode(kd.PCM_IN, struct.pack("<4500h", *[20000] * 4500)))   # too big
    await a.send_bytes(kd.encode(kd.PCM_IN, struct.pack("<32h", *[5000] * 32)))        # a frame it may send
    assert (kd.LED, b"\x01\x02\x03") not in await _frames(b, 0.5)
    await a.send_str(json.dumps({"console_in": "i2sstat"}))
    line = await next_json(a, lambda j: j.get("console", "").startswith("i2s_in "))
    peak = int(re.search(r"peak (\d+)", line["console"]).group(1))
    assert 5000 - 3 <= peak <= 5000 + 3, line
    await close(a)
    await close(b)


async def test_console_both_ways(sim):
    a = await ws(sim)
    await a.send_str(json.dumps({"console_in": "hello"}))
    await next_json(a, lambda j: j.get("console") == "echo: hello")
    await sim.said("echo: hello")                      # and the terminal shows it
    await close(a)


async def test_a_foreign_origin_is_refused(sim):
    async with aiohttp.ClientSession() as s:
        with pytest.raises(aiohttp.WSServerHandshakeError) as e:
            await s.ws_connect(f"{sim.url}ws", headers={"Origin": "https://evil.example"})
        assert e.value.status == 403
        for origin in (f"http://127.0.0.1:{sim.port}", f"http://localhost:{sim.port}"):
            w = await s.ws_connect(f"{sim.url}ws", headers={"Origin": origin})
            await w.close()
        r = await s.post(f"{sim.url}api/install", data=b"\xe9" + bytes(100),
                         headers={"Origin": "https://evil.example"})
        assert r.status == 403


async def test_a_foreign_upload_is_refused_before_its_body_is_read(sim):
    """A site's form posting 16 MB: refused on its headers, not after
    kode-sim has taken in the whole body."""
    for api in ("install", "mailbox"):
        reader, writer = await asyncio.open_connection("127.0.0.1", sim.port)
        writer.write((f"POST /api/{api} HTTP/1.1\r\nHost: 127.0.0.1:{sim.port}\r\n"
                      "Origin: https://evil.example\r\nContent-Type: application/octet-stream\r\n"
                      "Content-Length: 16000000\r\n\r\n").encode() + b"\xe9" * 1000)
        await writer.drain()
        status = await asyncio.wait_for(reader.readline(), 3)       # the rest of the body never comes
        assert b" 403 " in status, (api, status)
        writer.close()


async def test_a_page_that_never_reads_does_not_stall_the_firmware(sim):
    stuck = await ws(sim)                         # connected, never read
    live = await ws(sim)
    await live.send_str(json.dumps({"console_in": "flood 200"}))   # ~80 MB of screen
    await next_json(live, lambda j: j.get("console") == "flooded", timeout=30)
    await live.send_str(json.dumps({"console_in": "still here"}))
    await next_json(live, lambda j: j.get("console") == "echo: still here", timeout=5)
    await close(stuck)
    await close(live)


async def test_a_page_that_fell_behind_still_learns_the_state(sim):
    stuck = await ws(sim)                          # connected, not reading
    live = await ws(sim)
    await live.send_str(json.dumps({"console_in": "flood 600"}))
    await next_json(live, lambda j: j.get("console") == "flooded", timeout=60)
    await live.send_str(json.dumps({"console_in": "poweroff"}))
    await next_json(live, lambda j: j.get("state") == "off")
    await next_json(stuck, lambda j: j.get("state") == "off", timeout=30)
    await close(stuck)
    await close(live)


async def test_a_page_that_never_reads_is_let_go_once_its_console_backlog_passes_its_bound(sim):
    """The console is never skipped, so a page that never reads would hold
    all of it: past a bound it is disconnected instead, and a page that
    connects again is sent the whole picture."""
    stuck = await ws(sim)                           # connected, never read
    ctl = await ws(sim)
    await ctl.send_str(json.dumps({"console_in": "spam 200000"}))   # ~25 MB of console
    await close(ctl)
    await sim.said("spammed", timeout=60)

    async def closed():
        async for _ in stuck:
            pass
    await asyncio.wait_for(closed(), 60)
    assert stuck.closed
    again = await ws(sim)
    await next_json(again, lambda j: j.get("state") == "running")
    await close(again)
    await stuck.session.close()


# ---- the board's chips, in-process -------------------------------------------------

async def test_the_page_s_buttons_and_finger_reach_the_board_s_chips(sim):
    """TOP is a P4 pin, driven by the chips; the other five and the finger
    are the expander's and the touch controller's, read over I2C."""
    a = await ws(sim)
    await a.send_bytes(kd.encode(kd.BUTTONS, bytes([kd.BTN_TOP | kd.BTN_LEFT])))
    driven: set = set()
    await next_json(a, lambda j: driven.add(j.get("console")) or {"gpio 35 0", "gpio 6 0"} <= driven)
    await a.send_str(json.dumps({"console_in": "i2c 20 00 1"}))
    await next_json(a, lambda j: j.get("console") == "i2c 20 00: fb")          # P2, PAD_LEFT, low
    await a.send_bytes(kd.encode(kd.TOUCH, struct.pack("<BHH", 1, 300, 401)))
    await a.send_str(json.dumps({"console_in": "i2c 15 01 6"}))
    await next_json(a, lambda j: j.get("console") == "i2c 15 01: 0001812c0191")
    await close(a)


async def test_the_page_s_motion_reaches_the_imu_and_the_magnetometer_and_a_reboot_keeps_it(sim):
    """The QMI8658A (0x6A) and the QMC5883P (0x2C) measure what the page says,
    in the scales the drivers chose; a board that boots again is told it again."""
    a = await ws(sim)
    motion = struct.pack("<10i", 500, -250, 866, 30_000, 0, -64_000, 100, -2000, 300, 215)
    await a.send_bytes(kd.encode(kd.MOTION, motion))
    await a.send_str(json.dumps({"console_in": "i2c 6a 00 1"}))
    await next_json(a, lambda j: j.get("console") == "i2c 6a 00: 05")
    for cmd in ("i2cw 6a 02 58", "i2cw 6a 03 23", "i2cw 6a 04 53", "i2cw 6a 08 03"):   # kdrv_qmi8658_create()
        await a.send_str(json.dumps({"console_in": cmd}))
    await a.send_str(json.dumps({"console_in": "i2c 6a 33 e"}))
    got = await next_json(a, lambda j: str(j.get("console", "")).startswith("i2c 6a 33: "))
    raw = bytes.fromhex(got["console"].split(": ")[1])
    t, ax, ay, az, gx, gy, gz = struct.unpack("<7h", raw)
    assert (ax, ay, az) == (2048, -1024, 3547)          # 4096 LSB/g
    assert (gx, gy, gz) == (1920, 0, -4096)             # 64 LSB/dps
    assert t == 21 * 256 + 128                          # 21.5 C, 8.8 fixed point
    await a.send_str(json.dumps({"console_in": "i2c 2c 00 1"}))
    await next_json(a, lambda j: j.get("console") == "i2c 2c 00: 80")
    await a.send_str(json.dumps({"console_in": "i2cw 2c 0a 07"}))                        # continuous
    await asyncio.sleep(0.2)
    await a.send_str(json.dumps({"console_in": "i2c 2c 01 6"}))
    got = await next_json(a, lambda j: str(j.get("console", "")).startswith("i2c 2c 01: "))
    assert struct.unpack("<3h", bytes.fromhex(got["console"].split(": ")[1])) == (100, -2000, 300)
    await a.send_str(json.dumps({"console_in": "reboot"}))                                # a panic's reboot: a new board
    await next_json(a, lambda j: j.get("console") == "fake: reboot")
    await asyncio.sleep(0.5)
    for cmd in ("i2cw 6a 02 58", "i2cw 6a 03 23", "i2cw 6a 08 01", "i2c 6a 33 e"):
        await a.send_str(json.dumps({"console_in": cmd}))
    got = await next_json(a, lambda j: str(j.get("console", "")).startswith("i2c 6a 33: "))
    assert struct.unpack("<7h", bytes.fromhex(got["console"].split(": ")[1]))[1:4] == (2048, -1024, 3547)
    await close(a)


async def test_the_chips_led_reaches_the_page(sim):
    """The KTD2026 lit as kdrv_led lights it: OUT1 (green) at full current."""
    a = await ws(sim)
    for cmd in ("i2cw 30 00 18", "i2cw 30 04 01", "i2cw 30 06 27"):
        await a.send_str(json.dumps({"console_in": cmd}))
    d = kd.Decoder()

    async def lit():
        async for m in a:
            if m.type == aiohttp.WSMsgType.BINARY:
                if any(t == kd.LED and p == b"\x00\xff\x00" for t, p in d.feed(m.data)):
                    return
    await asyncio.wait_for(lit(), 10)
    b = await ws(sim)                                   # and a page that comes later sees it lit
    assert await next_frame(b, kd.LED) == b"\x00\xff\x00"
    await close(a)
    await close(b)


async def _pcm_out(w, timeout=5):
    """The next PCM_OUT's rate, channels and samples."""
    p = await next_frame(w, kd.PCM_OUT, timeout)
    rate, ch = struct.unpack_from("<IB", p)
    return rate, ch, list(memoryview(p[5:]).cast("h"))


async def test_the_chips_are_the_codec_on_the_i2s_data_pads(sim):
    """DSDIN (38), what the P4 sends on, and ASDOUT (33), what it reads."""
    await sim.said("fake: i2s dout 38 din 33")


async def _no_sound(w, seconds=0.5):
    return kd.PCM_OUT not in [t for t, _ in await _frames(w, seconds)]


async def test_the_speaker_reaches_every_page_mono_at_the_dac_s_volume(sim):
    """What the chip plays on I2S comes out as the board's one speaker plays
    it: the left slot, at REG32's volume; silence (a new board's codec, or
    REG31 muting it) sends nothing. The codec, a chip of its own, keeps its
    volume across a reset."""
    a = await ws(sim)
    b = await ws(sim)
    await a.send_str(json.dumps({"console_in": "play 8"}))
    assert await _no_sound(a), "a new board's codec is silent"
    await a.send_str(json.dumps({"console_in": "i2cw 18 32 bf"}))     # 0 dB
    await next_json(a, lambda j: j.get("console") == "i2cw 18 32: ok")
    await a.send_str(json.dumps({"console_in": "play 8"}))
    assert await _pcm_out(a) == (48000, 1, [1000] * 8)
    assert await _pcm_out(b) == (48000, 1, [1000] * 8), "every page"
    await a.send_str(json.dumps({"console_in": "i2cw 18 31 60"}))     # the DAC's mute bits
    await next_json(a, lambda j: j.get("console") == "i2cw 18 31: ok")
    await a.send_str(json.dumps({"console_in": "play 8"}))
    assert await _no_sound(a), "muted"
    await a.send_str(json.dumps({"console_in": "i2cw 18 31 00"}))
    await a.send_str(json.dumps({"power": "reset"}))
    await _console_says(a, "fake: reset")
    await a.send_str(json.dumps({"console_in": "play 8"}))
    assert await _pcm_out(a) == (48000, 1, [1000] * 8), "the codec kept its volume"
    await close(a)
    await close(b)


async def test_the_mac_s_microphone_reaches_the_chip_s_receiver(sim):
    """A page turns the microphone on; kode-sim asks every page for it at the
    rate the firmware's receiver runs at, and what it hears is fed to the
    receiver, in the left slot, ahead of emulated time."""
    a = await ws(sim)
    await a.send_str(json.dumps({"mic": True}))
    await next_frame(a, kd.MIC_START)
    await a.send_str(json.dumps({"console_in": "rxformat 16000 2 16"}))
    assert await next_frame(a, kd.MIC_START) == struct.pack("<IB", 16000, 1)
    for _ in range(10):
        await a.send_bytes(kd.encode(kd.PCM_IN, struct.pack("<160h", *[10000] * 160)))
    await asyncio.sleep(0.3)
    await a.send_str(json.dumps({"console_in": "i2sstat"}))
    line = await next_json(a, lambda j: j.get("console", "").startswith("i2s_in "))
    frames, peak = (int(x) for x in re.search(r"frames (\d+) peak (\d+)", line["console"]).groups())
    assert frames >= 16000 * 0.25, line                # fed all along, 5 ms ahead
    assert 10000 - 3 <= peak <= 10000 + 3, line
    await close(a)


async def test_the_microphone_switch_is_every_page_s_and_survives_a_reset(sim):
    """{"mic": true} from one page is MIC_START to all, and to a page that
    comes later (a reload); off, it is MIC_STOP and a later page is not told.
    It is the page's, not the firmware's: a reset leaves it on, the board
    going off turns it off."""
    a = await ws(sim)
    await a.send_str(json.dumps({"mic": True}))
    assert await next_frame(a, kd.MIC_START) == struct.pack("<IB", 48000, 1)   # no receiver yet: 48 kHz
    b = await ws(sim)
    assert await next_frame(b, kd.MIC_START) == struct.pack("<IB", 48000, 1)
    await a.send_str(json.dumps({"console_in": "rxformat 16000 2 16"}))
    assert await next_frame(b, kd.MIC_START) == struct.pack("<IB", 16000, 1), "the receiver's rate, when it says"
    await a.send_str(json.dumps({"console_in": "rxformat 16000 2 32"}))   # the same rate: not asked again
    assert kd.MIC_START not in [t for t, _ in await _frames(b, 0.5)]
    await a.send_str(json.dumps({"mic": False}))
    await next_frame(b, kd.MIC_STOP)
    c = await ws(sim)
    assert kd.MIC_START not in [t for t, _ in await _frames(c, 1)]
    await a.send_str(json.dumps({"mic": True}))
    await next_frame(a, kd.MIC_START)
    await a.send_str(json.dumps({"power": "reset"}))
    await _console_says(a, "fake: reset")
    d = await ws(sim)
    assert await next_frame(d, kd.MIC_START) == struct.pack("<IB", 16000, 1)
    for w in (a, b, c, d):
        await close(w)


async def test_the_battery_from_the_page_reaches_the_gauge_and_every_page(sim):
    a = await ws(sim)
    b = await ws(sim)
    await a.send_bytes(kd.encode(kd.BATTERY, bytes([42, 0])))
    await next_json(b, lambda j: j.get("battery") == {"percent": 42, "usb": False})
    await a.send_str(json.dumps({"console_in": "i2c 64 04 1"}))      # the CW2217's SOC
    await next_json(a, lambda j: j.get("console") == "i2c 64 04: 2a")         # 42 %
    await close(a)
    await close(b)


async def test_the_charger_switching_the_board_off_powers_it_off(sim, fake_emu):
    """kodeOS's `sleep`: the BATFET opened on battery - the board is off,
    and BOT (the charger's QON) brings it back."""
    a = await ws(sim)
    await next_json(a, lambda j: j.get("state") == "running")
    await a.send_bytes(kd.encode(kd.BATTERY, bytes([50, 0])))
    await a.send_str(json.dumps({"console_in": "i2cw 6b 09 20"}))
    await next_frame(a, kd.POWER_OFF)
    await next_json(a, lambda j: j.get("state") == "off")
    await a.send_bytes(kd.encode(kd.BUTTONS, bytes([kd.BTN_BOT])))
    await next_json(a, lambda j: j.get("state") == "running")
    assert len(fake_emu.read_text().splitlines()) == 2
    await close(a)


# ---- power, reset, crashes -----------------------------------------------------------

async def test_power_off_then_button_relaunches(sim, fake_emu):
    a = await ws(sim)
    await next_json(a, lambda j: j.get("state") == "running")
    await a.send_str(json.dumps({"console_in": "poweroff"}))
    await next_json(a, lambda j: j.get("state") == "off")
    await a.send_bytes(kd.encode(kd.BUTTONS, bytes([kd.BTN_BOT])))
    await next_json(a, lambda j: j.get("state") == "running")
    assert len(fake_emu.read_text().splitlines()) == 2
    await close(a)


async def test_emu_crash_reported(sim):
    a = await ws(sim)
    await next_json(a, lambda j: j.get("state") == "running")
    await a.send_str(json.dumps({"console_in": "crash"}))
    j = await next_json(a, lambda j: j.get("state") == "crashed")
    assert "Guru Meditation" in j["detail"] and "esp-emu exited with 1" in j["detail"]
    assert "the firmware crashed" in sim.stderr()
    await a.send_str(json.dumps({"power": "on"}))
    await next_json(a, lambda j: j.get("state") == "running")
    await close(a)


async def test_two_power_ons_at_once_start_one_emulator(exe, fake_emu, tmp_path, monkeypatch):
    monkeypatch.setenv("FAKE_SLOW_START", "1")          # every boot slow: the second press lands mid-boot
    sim = await start(exe, tmp_path)
    try:
        a = await ws(sim)
        await next_json(a, lambda j: j.get("state") == "running")
        await a.send_str(json.dumps({"console_in": "crash"}))
        await next_json(a, lambda j: j.get("state") == "crashed")
        b = await ws(sim)                               # a second page, pressing at the same time
        await asyncio.gather(a.send_str(json.dumps({"power": "on"})),
                             b.send_bytes(kd.encode(kd.BUTTONS, bytes([kd.BTN_BOT]))))
        await next_json(a, lambda j: j.get("state") == "running")
        await asyncio.sleep(2)
        assert len(fake_emu.read_text().splitlines()) == 2      # the first boot, and one more
        await close(a)
        await close(b)
    finally:
        sim.stop()


async def test_the_page_s_reset_reboots_the_firmware(sim):
    """The page's Reset: a chip reset, as the EN line does on the board - the
    firmware boots again, and the simulator never goes off in between."""
    a = await ws(sim)
    await _past_history(a)
    states = []
    await a.send_str(json.dumps({"power": "reset"}))
    await next_json(a, lambda j: states.append(j["state"]) if "state" in j else j.get("console") == "fake: reset")
    await _console_says(a, "I (629) app_init: Project name:     kodeos")
    assert "off" not in states, states
    await close(a)


async def _frames(w, seconds):
    """The KD frames a page gets in the next @seconds."""
    d, got = kd.Decoder(), []

    async def scan():
        async for m in w:
            if m.type == aiohttp.WSMsgType.BINARY:
                got.extend(d.feed(m.data))
        raise _closed(w, f"{seconds} s of frames")      # no frames because no page: not an answer
    try:
        await asyncio.wait_for(scan(), seconds)
    except (asyncio.TimeoutError, TimeoutError):
        pass
    return got


async def test_a_boot_clears_the_screen_on_every_page(sim):
    """HELLO starts the screen dark, as the board's does: a page already
    there is sent the dark screen, as one that joins later is."""
    a = await ws(sim)
    whole = await next_frame(a, kd.FB_RECT)               # the picture, as it joins
    assert struct.unpack_from("<HHHH", whole) == (0, 0, 410, 502) and whole[8:10] == b"\xff\xff"
    await _past_history(a)
    await a.send_str(json.dumps({"power": "reset"}))
    while True:
        p = await next_frame(a, kd.FB_RECT)
        if struct.unpack_from("<HHHH", p) == (0, 0, 410, 502):
            break
    assert not any(p[8:]), "the screen a boot starts with is dark"
    await close(a)


async def _booted_again(a, after):
    """The firmware's next boot, as the bridge saw it: its HELLO answered."""
    await _console_says(a, after)
    await _console_starts(a, f"rx {kd.TIME} 8 ")


async def test_a_reset_that_cuts_a_frame_short_does_not_hide_the_next_boot(sim):
    """The page's reset lands mid-frame: the frame is never finished, and the
    next boot's HELLO - which would just finish it - is still seen: the
    bridge forgets the cut frame as it resets the chip."""
    a = await ws(sim)
    await _past_history(a)
    await a.send_str(json.dumps({"console_in": "cut 17"}))
    await _console_says(a, "cut")
    await a.send_str(json.dumps({"power": "reset"}))
    await _booted_again(a, "fake: reset")
    await close(a)


async def test_a_panic_s_reboot_after_a_cut_frame_is_seen(sim):
    """A reboot nobody asked for (a panic) after half a screen frame: the
    HELLO among the bytes the cut frame waits for starts the stream again."""
    a = await ws(sim)
    await _past_history(a)
    await a.send_str(json.dumps({"console_in": "cut 500000"}))
    await _console_says(a, "cut")
    await a.send_str(json.dumps({"console_in": "reboot"}))
    await _booted_again(a, "fake: reboot")
    await close(a)


async def test_bot_and_left_held_a_second_reset_like_the_board(sim):
    a = await ws(sim)
    await _past_history(a)
    await a.send_bytes(kd.encode(kd.BUTTONS, bytes([kd.BTN_BOT | kd.BTN_LEFT])))
    await _console_says(a, "fake: reset", timeout=3)
    await a.send_bytes(kd.encode(kd.BUTTONS, bytes([0])))
    await close(a)


async def test_bot_and_left_let_go_early_do_not_reset(sim):
    a = await ws(sim)
    await _past_history(a)
    await a.send_bytes(kd.encode(kd.BUTTONS, bytes([kd.BTN_BOT | kd.BTN_LEFT])))
    await asyncio.sleep(0.5)
    await a.send_bytes(kd.encode(kd.BUTTONS, bytes([kd.BTN_BOT])))
    with pytest.raises((asyncio.TimeoutError, TimeoutError)):
        await _console_says(a, "fake: reset", timeout=1.5)
    await close(a)


async def test_run_exits_nonzero_when_the_firmware_crashes(exe, fake_emu, tmp_path, monkeypatch):
    monkeypatch.setenv("FAKE_CRASH_AFTER", "0.5")
    sim = await start(exe, tmp_path, "--exit-on-crash", wait=False)
    try:
        rc = await asyncio.to_thread(sim.proc.wait, 15)
    finally:
        sim.stop()
    assert rc == 2, sim.stderr()
    assert "the firmware crashed - esp-emu exited with 1" in sim.stderr()
    assert not list((sim.cache / "running").glob("*.json")), "the note outlived the simulator"


async def test_without_an_esp_emu_run_says_so(exe, tmp_path, monkeypatch):
    monkeypatch.setenv("KODE_SIM_ESP_EMU", str(tmp_path / "no-esp-emu"))
    sim = await start(exe, tmp_path, wait=False)
    rc = await asyncio.to_thread(sim.proc.wait, 15)
    assert rc == 1
    assert "no-esp-emu" in sim.stderr()


async def test_sigterm_stops_the_simulator_and_its_emulator(sim):
    """Killed with SIGTERM (or SIGHUP, the terminal closing), `kode-sim run`
    shuts down as on Ctrl-C: no esp-emu left behind, no note."""
    children = subprocess.run(["pgrep", "-P", str(sim.proc.pid)], capture_output=True, text=True).stdout.split()
    assert children, "no esp-emu started"
    assert sim.stop(signal.SIGTERM) == 0
    time.sleep(0.5)
    alive = [c for c in children if subprocess.run(["kill", "-0", c], capture_output=True).returncode == 0]
    assert not alive, f"esp-emu {alive} outlived kode-sim"
    assert sim.note() is None


async def test_sigterm_mid_boot_leaves_no_esp_emu(exe, fake_emu, tmp_path, monkeypatch):
    monkeypatch.setenv("FAKE_SLOW_START", "3")          # still booting when the signal comes
    sim = await start(exe, tmp_path, wait=False)
    children: list[str] = []
    for _ in range(100):
        children = subprocess.run(["pgrep", "-P", str(sim.proc.pid)], capture_output=True, text=True).stdout.split()
        if children:
            break
        await asyncio.sleep(0.05)
    assert children, "no esp-emu started"
    assert sim.stop(signal.SIGTERM) == 0
    time.sleep(0.5)
    alive = [c for c in children if subprocess.run(["kill", "-0", c], capture_output=True).returncode == 0]
    for c in alive:
        os.kill(int(c), signal.SIGKILL)
    assert not alive, f"esp-emu {alive} outlived kode-sim"


async def test_sighup_and_sigint_stop_it_too(exe, fake_emu, tmp_path):
    for sig in (signal.SIGHUP, signal.SIGINT):
        sim = await start(exe, tmp_path)
        assert sim.stop(sig) == 0, sig
        assert sim.note() is None


def _children(pid) -> list[str]:
    return subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()


def _alive(pids) -> list[str]:
    return [c for c in pids if subprocess.run(["kill", "-0", c], capture_output=True).returncode == 0]


async def _a_new_child(pid, old=()) -> list[str]:
    for _ in range(200):
        new = [c for c in _children(pid) if c not in old]
        if new:
            return new
        await asyncio.sleep(0.05)
    raise AssertionError("no esp-emu started")


async def test_sigterm_while_a_page_boots_it_leaves_no_esp_emu(exe, fake_emu, tmp_path, monkeypatch):
    """The page's power button pressed, the boot still under way, and the
    simulator told to stop: what that boot started goes too."""
    monkeypatch.setenv("FAKE_SLOW_START", "2")
    sim = await start(exe, tmp_path)
    try:
        a = await ws(sim)
        await a.send_str(json.dumps({"console_in": "crash"}))
        await next_json(a, lambda j: j.get("state") == "crashed")
        first = _children(sim.proc.pid)
        await a.send_str(json.dumps({"power": "on"}))
        booting = await _a_new_child(sim.proc.pid, first)
        assert sim.stop(signal.SIGTERM) == 0
        time.sleep(0.5)
        alive = _alive(booting)
        for c in alive:
            os.kill(int(c), signal.SIGKILL)
        assert not alive, f"esp-emu {alive} outlived kode-sim"
        assert "panicked" not in sim.stderr(), sim.stderr()
    finally:
        sim.stop()


async def test_sigterm_stops_a_review_and_its_emulator(exe, fake_emu, tmp_path, monkeypatch):
    """`kode-sim review` stopped half-way (SIGTERM, Ctrl-C, a closed
    terminal): its esp-emu goes with it."""
    monkeypatch.setenv("FAKE_OUTLIVE_UART", "1")
    kodeos = tmp_path / "kodeos"
    kodeos.mkdir()
    (kodeos / "merged-binary.bin").write_bytes(b"\xe9kodeOS" + bytes(1000))
    (kodeos / "kodeos.elf").write_bytes(b"elf")
    app = tmp_path / "app.bin"
    app.write_bytes(b"\xe9" + bytes(200) + b"kemu_rx" + bytes(256))
    scenario = tmp_path / "scenario.json"
    scenario.write_text(json.dumps({"steps": [{"wait": 60}], "timeout_s": 120}))
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        proc = subprocess.Popen([str(exe), "review", str(app), str(scenario), "--out", str(tmp_path / "out"),
                                 "--kodeos", str(kodeos)],
                                env={**os.environ, "KODE_SIM_CACHE": str(tmp_path / "cache")},
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            emus = await _a_new_child(proc.pid)
            await asyncio.sleep(1)                       # into the scenario
            proc.send_signal(sig)
            rc = await asyncio.to_thread(proc.wait, 15)
            time.sleep(0.5)
            alive = _alive(emus)
            for c in alive:
                os.kill(int(c), signal.SIGKILL)
            assert not alive, f"{sig}: esp-emu {alive} outlived kode-sim review"
            assert rc == 128 + sig, (sig, rc)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()


# ---- esp-emu's command line ---------------------------------------------------------

def _c5_args(fake_emu):
    """The C5's esp-emu's arguments, its last launch's."""
    return (fake_emu.parent / (fake_emu.name + ".c5")).read_text().splitlines()[-1].split()


async def test_esp_emu_runs_as_a_kode_dot(sim, fake_emu, tmp_path):
    args = fake_emu.read_text().split()
    assert args[:2] == ["--chip", "esp32p4"]
    assert args[args.index("--firmware") + 1] == str(tmp_path / "m.bin")
    assert args[args.index("--elf") + 1] == str(tmp_path / "a.elf")
    assert args[args.index("--psram-size") + 1] == "32M"
    for flag in ("--uart1-tcp", "--control-tcp", "--cosim-tcp"):
        assert args[args.index(flag) + 1].startswith("127.0.0.1:"), flag
    assert args[args.index("--sd") + 1] == f"{tmp_path / 'card.img'},slot=0"
    assert args[args.index("--hosted-reset-gpio") + 1] == "23"
    # The P4's own network shut: left out, esp-emu on Linux opens TAP tap0.
    assert args[args.index("--net") + 1] == "user,restrict=yes"
    assert "--save-state" not in args
    # The C5 beside it, on the other end of the SDIO socket, with the network.
    c5 = _c5_args(fake_emu)
    assert c5[:2] == ["--chip", "esp32c5"]
    sock = args[args.index("--hosted") + 1].removeprefix("bridge:host:")
    assert c5[c5.index("--hosted") + 1] == f"bridge:slave:{sock}"
    assert c5[c5.index("--net") + 1] == "user"
    assert c5[c5.index("--wifi-ssid") + 1] == "kode-sim"


async def test_forwarded_ports_reach_esp_emu_as_hostfwd(exe, fake_emu, tmp_path):
    """`kode-sim run --forward 25565 --forward 8080:80`: the Mac's localhost
    ports into the Kode Dot's, through the user-mode network of the C5's
    esp-emu, whose WiFi the Kode Dot is on."""
    sim = await start(exe, tmp_path, "--forward", "25565", "--forward", "8080:80")
    try:
        c5 = _c5_args(fake_emu)
        assert c5[c5.index("--net") + 1] == \
            "user,hostfwd=tcp:127.0.0.1:25565-:25565,hostfwd=tcp:127.0.0.1:8080-:80", c5
        assert "127.0.0.1:8080 on this Mac reaches the Kode Dot's port 80" in sim.stderr()
    finally:
        sim.stop()


async def test_a_bad_forward_is_refused(exe, tmp_path):
    for bad in ("x", "0", "70000", "80:x"):
        r = subprocess.run([str(exe), "run", "--firmware", "m", "--elf", "e", "--forward", bad],
                           capture_output=True, text=True)
        assert r.returncode == 2 and "PORT" in r.stderr, (bad, r.stderr)


async def test_the_c5_comes_up_unless_it_is_dead(exe, fake_emu, tmp_path):
    """The C5's esp-emu beside the P4's by default; with `--no-wifi` none,
    and the P4's SDIO link finds no card - as on a board whose C5 is dead."""
    c5_log = fake_emu.parent / (fake_emu.name + ".c5")
    for args, alive in (((), True), (("--no-wifi",), False)):
        c5_log.unlink(missing_ok=True)
        sim = await start(exe, tmp_path, *args)
        try:
            await sim.said(f"rx {kd.TIME} 8 ")
            p4 = fake_emu.read_text().splitlines()[-1].split()
            assert ("--hosted" in p4) == alive, p4
            assert c5_log.exists() == alive
        finally:
            sim.stop()


async def test_a_board_image_s_console_is_esp_emu_s_stdin(exe, fake_emu, tmp_path, monkeypatch):
    """A firmware built for the board has no kemu, so it never says HELLO:
    what the page types reaches its USB-Serial/JTAG, esp-emu's stdin."""
    monkeypatch.setenv("FAKE_BOARD_IMAGE", "1")
    sim = await start(exe, tmp_path)
    try:
        a = await ws(sim)
        await a.send_str(json.dumps({"console_in": "help"}))
        await _console_says(a, "stdin: help", timeout=10)
        await close(a)
    finally:
        sim.stop()


async def test_after_every_hello_the_firmware_is_told_the_time(sim):
    """The board's P4 keeps the wall clock in its RTC across resets, so the
    bridge gives it the Mac's time after every HELLO."""
    await sim.said(f"rx {kd.TIME} 8 ")
    line = next(l for l in sim.printed() if l.startswith(f"rx {kd.TIME} 8 "))
    unix_us = struct.unpack("<Q", bytes.fromhex(line.split()[3]))[0]
    assert abs(unix_us / 1e6 - time.time()) < 5


# ---- the serial port -------------------------------------------------------------------

async def test_the_serial_port_is_the_console(sim):
    """kcli.py -p <port> talks to the simulated Kode Dot as to the board's
    USB-Serial/JTAG: bytes in are console input, console lines come out."""
    assert os.path.exists(sim.serial)
    assert sim.serial in sim.stderr()
    fd = open_serial(sim.serial)
    try:
        os.write(fd, b"hello over serial\r\n")
        got = await serial_read_until(fd, b"echo: hello over serial")
        assert b"echo: hello over serial\r\n" in got, got
    finally:
        os.close(fd)


async def test_the_serial_port_carries_binary_whole(sim):
    fd = open_serial(sim.serial)
    try:
        data = bytes(range(256)) * 40
        os.write(fd, b"count " + str(len(data)).encode() + b"\n")
        await serial_read_until(fd, b"counting")
        await serial_write(fd, data)
        got = await serial_read_until(fd, b"counted ")
        assert f"counted {len(data)} crc {zlib.crc32(data)}".encode() in got, got
    finally:
        os.close(fd)


# ---- kode-sim install, and the page's upload ------------------------------------------------

async def test_run_announces_itself_and_install_finds_it(exe, fake_emu, tmp_path):
    """The note `kode-sim run` leaves while it runs: its page and its serial
    port, which `kode-sim install` finds and installs over. It goes when
    the simulator does."""
    sim = await start(exe, tmp_path)
    try:
        found = sim.note()
        assert found["url"] == sim.url and found["pid"] == sim.proc.pid and os.path.exists(found["serial"]), found
        app = tmp_path / "app.bin"
        app.write_bytes(b"\xe9" + os.urandom(4096 * 3))
        r = await asyncio.to_thread(subprocess.run, [str(exe), "install", str(app)],
                                    env={**os.environ, "KODE_SIM_CACHE": str(sim.cache)},
                                    capture_output=True, text=True, timeout=60)
        assert r.returncode == 0 and "installed" in r.stderr, r.stderr
    finally:
        sim.stop()
    assert sim.note() is None, "the note outlived the simulator"


async def test_an_app_uploaded_from_the_page_is_installed(sim):
    image = b"\xe9" + bytes(range(256)) * 40
    a = await ws(sim)
    async with aiohttp.ClientSession() as s:
        r = await s.post(f"{sim.url}api/install", data=image,
                         headers={"Origin": f"http://127.0.0.1:{sim.port}",
                                  "Content-Type": "application/octet-stream"})
        assert r.status == 200, await r.text()
        assert await r.json() == {"ok": True, "bytes": len(image)}
        await next_json(a, lambda j: str(j.get("console", "")).startswith(
            f"[kode-sim] installed {len(image)} bytes"))
        await _console_says(a, "I (629) app_init: Project name:     app")      # kodeOS boots it
        r = await s.post(f"{sim.url}api/install", data=b"not an app")
        assert r.status == 400
        assert (await r.json())["ok"] is False
    await close(a)


async def test_a_mailbox_esp_emu_could_not_write_is_not_reset_into(exe, fake_emu, tmp_path, monkeypatch):
    """write-region answered `err`: the upload fails, saying so, and the chip
    is not reset into a mailbox that never got the app."""
    monkeypatch.setenv("FAKE_WRITE_REGION_ERR", "1")
    sim = await start(exe, tmp_path)
    resets = lambda: sum("fake: reset" in l for l in sim.printed())     # noqa: E731
    try:
        await sim.said(f"rx {kd.TIME} 8 ")                   # booted, its launch's reset done
        before = resets()
        async with aiohttp.ClientSession() as s:
            r = await s.post(f"{sim.url}api/mailbox", data=b"\xe9" + bytes(5000))
            assert r.status == 422, await r.text()
            assert "write-region" in (await r.json())["error"]
        await asyncio.sleep(0.5)
        assert resets() == before, sim.printed()[-5:]
    finally:
        sim.stop()


async def test_an_install_whose_page_left_still_holds_off_a_second(exe, fake_emu, tmp_path, monkeypatch):
    """The page that uploaded gave up waiting: the install goes on, and until
    it ends a second one is refused, not run beside it."""
    monkeypatch.setenv("FAKE_INSTALL_SLOW", "2")
    sim = await start(exe, tmp_path)
    try:
        a = await ws(sim)
        image = b"\xe9" + bytes(5000)
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=0.5)) as s:
            with pytest.raises((asyncio.TimeoutError, TimeoutError)):
                await s.post(f"{sim.url}api/install", data=image)
        async with aiohttp.ClientSession() as s:
            for api in ("install", "mailbox"):
                r = await s.post(f"{sim.url}api/{api}", data=image)
                assert r.status == 409, (api, await r.text())
                assert "already under way" in (await r.json())["error"]
        await next_json(a, lambda j: str(j.get("console", "")).startswith(
            f"[kode-sim] installed {len(image)} bytes"), timeout=10)
        await close(a)
    finally:
        sim.stop()


async def test_a_mailbox_whose_page_left_is_still_finished_and_cleaned_up(exe, fake_emu, tmp_path, monkeypatch):
    """The page gave up waiting on a slow write-region: the app still goes
    into the mailbox, the chip is still reset, and the files it went
    through are gone."""
    monkeypatch.setenv("FAKE_WRITE_REGION_SLOW", "1.5")
    tmp = tmp_path / "tmp"
    tmp.mkdir()
    monkeypatch.setenv("TMPDIR", str(tmp))
    sim = await start(exe, tmp_path)
    try:
        a = await ws(sim)
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=0.5)) as s:
            with pytest.raises((asyncio.TimeoutError, TimeoutError)):
                await s.post(f"{sim.url}api/mailbox", data=b"\xe9" + bytes(5000))
        await _console_says(a, "fake: reset", timeout=10)
        await _console_says(a, "!N 4431 kaos.appmgr dev app 'app' v1 adopted from the mailbox")
        await asyncio.sleep(0.2)
        assert not list(tmp.glob("kode-sim-mailbox-*")), list(tmp.iterdir())
        await close(a)
    finally:
        sim.stop()


async def test_an_upload_kodeos_refuses_says_why(exe, fake_emu, tmp_path, monkeypatch):
    monkeypatch.setenv("FAKE_INSTALL_REFUSE", "1")
    sim = await start(exe, tmp_path)
    try:
        async with aiohttp.ClientSession() as s:
            r = await s.post(f"{sim.url}api/install", data=b"\xe9" + bytes(5000))
            assert r.status == 422
            assert "code=5" in (await r.json())["error"]
    finally:
        sim.stop()


async def test_an_upload_to_a_kode_dot_that_is_off_is_refused(sim):
    a = await ws(sim)
    await a.send_str(json.dumps({"console_in": "poweroff"}))
    await next_json(a, lambda j: j.get("state") == "off")
    async with aiohttp.ClientSession() as s:
        r = await s.post(f"{sim.url}api/install", data=b"\xe9" + bytes(100))
        assert r.status == 409
        assert await r.json() == {"ok": False, "error": "the Kode Dot is off"}
    await close(a)


# ---- the microSD --------------------------------------------------------------------------

def _sd_of(fake_emu):
    """The --sd of esp-emu's last launch, None for an empty slot."""
    args = fake_emu.read_text().splitlines()[-1].split()
    return args[args.index("--sd") + 1] if "--sd" in args else None


async def test_the_card_is_in_the_slot_and_the_page_s_switch_restarts_with_it(sim, fake_emu, tmp_path):
    """The card is esp-emu's (--sd, SDMMC slot 0). esp-emu changes no slot
    while it runs, so the page's "SD insertada" restarts the Kode Dot with
    the card out, then in, and every page hears the switch."""
    card = f"{tmp_path / 'card.img'},slot=0"
    assert _sd_of(fake_emu) == card
    a = await ws(sim)
    await next_json(a, lambda j: j.get("sd") is True)
    for sd, want in ((False, None), (True, card)):
        launches = len(fake_emu.read_text().splitlines())
        await a.send_str(json.dumps({"sd": sd}))
        await next_json(a, lambda j: j.get("sd") is sd)
        await _console_starts(a, "[kode-sim] the card ", timeout=10)
        for _ in range(100):
            if len(fake_emu.read_text().splitlines()) > launches:
                break
            await asyncio.sleep(0.1)
        assert _sd_of(fake_emu) == want
        await _console_starts(a, f"rx {kd.TIME} 8 ", timeout=15)
    await close(a)


async def test_no_sd_starts_with_the_slot_empty(exe, fake_emu, tmp_path):
    sim = await start(exe, tmp_path, "--no-sd")
    try:
        await sim.said(f"rx {kd.TIME} 8 ")
        assert _sd_of(fake_emu) is None
        a = await ws(sim)
        await next_json(a, lambda j: j.get("sd") is False)
        await close(a)
    finally:
        sim.stop()


async def test_a_card_that_is_not_there_is_made(exe, fake_emu, tmp_path):
    if not (shutil.which("mformat") or shutil.which("diskutil")):
        pytest.skip("no mtools (or diskutil) to format a card with")
    img = tmp_path / "new" / "card.img"
    sim = await start(exe, tmp_path, "--sd", str(img))
    try:
        assert img.stat().st_size == 1024 * 1024 * 1024
        assert img.read_bytes()[:512][510:512] == b"\x55\xaa"
        assert "making a card image" in sim.stderr()
    finally:
        sim.stop()


async def test_a_card_in_one_simulator_is_not_put_in_another(sim, exe, tmp_path):
    """Two simulators on one card image would each write its FAT over the
    other's: the second says whose card it is and how to give it its own."""
    second = await start(exe, tmp_path, cache=tmp_path / "cache2", wait=False)
    try:
        rc = await asyncio.to_thread(second.proc.wait, 15)
    finally:
        second.stop()
    assert rc == 1, second.stderr()
    assert str(tmp_path / "card.img") in second.stderr() and "--sd" in second.stderr(), second.stderr()
    a = await ws(sim)                                   # and the first one's card is still in
    await next_json(a, lambda j: j.get("sd") is True)
    await close(a)


# ---- what the tests of the firmware ask of it ------------------------------------------------

async def test_no_internet_lets_nothing_out_but_dns(exe, fake_emu, tmp_path):
    """`kode-sim run --no-internet`: a Kode Dot on a network with no way
    out, as a review's ("network": "none") - its clock has to come from its RTC."""
    sim = await start(exe, tmp_path, "--no-internet", "--forward", "8080:80")
    try:
        c5 = _c5_args(fake_emu)
        assert c5[c5.index("--net") + 1] == "user,restrict=yes,hostfwd=tcp:127.0.0.1:8080-:80", c5
    finally:
        sim.stop()


async def test_the_page_s_stats_count_the_bytes_that_crossed_uart1(sim):
    """api/stats: what crossed UART1 from the firmware, before the decoder -
    the simulator's bottleneck, which a page's frames (unpacked) do not show."""
    async with aiohttp.ClientSession() as s:
        before = (await (await s.get(f"{sim.url}api/stats")).json())["uart_bytes"]
        assert before > 0                                   # HELLO and the 2x2 square
        a = await ws(sim)
        await a.send_str(json.dumps({"console_in": "flood 1"}))
        await next_json(a, lambda j: j.get("console") == "flooded")
        after = (await (await s.get(f"{sim.url}api/stats")).json())["uart_bytes"]
        assert after - before >= 410 * 502 * 2, (before, after)
        r = await s.get(f"{sim.url}api/stats", headers={"Origin": "https://evil.example"})
        assert r.status == 403
        await close(a)


async def test_run_of_what_is_not_a_project_says_so(exe, tmp_path):
    r = subprocess.run([str(exe), "run", "--no-open", str(tmp_path / "nope")],
                       env={**os.environ, "KODE_SIM_CACHE": str(tmp_path / "cache")},
                       capture_output=True, text=True, timeout=60)
    assert r.returncode != 0
    assert str(tmp_path / "nope") in r.stderr, r.stderr


# ---- run as sim.start does not: a stdout of its own, a port of its choosing ------------------

async def _run(exe, tmp_path, *args, port=None, stdout=None):
    """`kode-sim run` on the fake, its stdout @stdout, its page on @port
    (kd.free_port() by default), up once its note is there."""
    port = kd.free_port() if port is None else port
    argv = [str(exe), "run", "--firmware", str(tmp_path / "m.bin"), "--elf", str(tmp_path / "a.elf"),
            "--port", str(port), "--no-open", "--sd", str(card(tmp_path)), *args]
    out, err = tmp_path / f"out-{port}", tmp_path / f"err-{port}"
    proc = subprocess.Popen(argv, env={**os.environ, "KODE_SIM_CACHE": str(tmp_path / "cache")},
                            stdin=subprocess.DEVNULL, stdout=stdout or out.open("wb"), stderr=err.open("wb"))
    sim = Sim(proc, port, tmp_path / "cache", out, err)
    for _ in range(400):
        if sim.note() is not None or proc.poll() is not None:
            break
        await asyncio.sleep(0.05)
    assert sim.note() is not None, f"kode-sim run never came up: {sim.stderr()}"
    return sim


async def test_a_closed_stdout_does_not_stop_the_console(exe, fake_emu, tmp_path):
    """The terminal's pipe closed (`kode-sim run | head`): the lines it can
    no longer print are lost there, and nowhere else."""
    r, w = os.pipe()
    os.close(r)                                          # nobody reads what it prints
    try:
        sim = await _run(exe, tmp_path, stdout=w)
    finally:
        os.close(w)
    try:
        a = await ws(sim)
        for word in ("one", "two"):
            await a.send_str(json.dumps({"console_in": word}))
            await _console_says(a, f"echo: {word}")
        await close(a)
        assert "panicked" not in sim.stderr(), sim.stderr()
    finally:
        sim.stop()


async def test_port_0_serves_the_page_on_a_port_the_mac_picks(exe, fake_emu, tmp_path):
    """`--port 0`: the port the Mac gave it is the one in the URL, the note
    and the Origin check."""
    sim = await _run(exe, tmp_path, port=0)
    try:
        url = sim.note()["url"]
        assert url.startswith("http://127.0.0.1:") and url != "http://127.0.0.1:0/", url
        assert f"the Kode Dot is at {url}" in sim.stderr()
        sim.port = int(url.rstrip("/").rsplit(":", 1)[1])
        a = await ws(sim, headers={"Origin": f"http://127.0.0.1:{sim.port}"})
        await next_json(a, lambda j: j.get("state") == "running")
        await close(a)
    finally:
        sim.stop()


# ---- run in the background of a terminal ------------------------------------------

_BACKGROUND = r'''
import os, pty, subprocess, sys, time
argv, env = sys.argv[1:], dict(os.environ)
master, slave = pty.openpty()
os.setsid()                                         # a session of its own, the pty its terminal
fd = os.open(os.ttyname(slave), os.O_RDWR)
os.tcsetpgrp(fd, os.getpgrp())                      # this process's group is the foreground one
p = subprocess.Popen(argv, stdin=fd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     process_group=0, env=env)      # `kode-sim run &`: a group in the background
time.sleep(4)
stat = subprocess.run(["ps", "-o", "stat=", "-p", str(p.pid)], capture_output=True, text=True).stdout.strip()
print(stat, flush=True)
p.terminate()
os.kill(p.pid, 18)                                  # SIGCONT, should it have been stopped
try:
    p.wait(10)
except subprocess.TimeoutExpired:
    p.kill()
'''


async def test_run_in_the_background_of_a_terminal_keeps_running(exe, fake_emu, tmp_path):
    """`kode-sim run &` from a shell: it reads the terminal (its console),
    which a background job may not, and the terminal's SIGTTIN would stop
    it; it keeps serving instead, and reads the terminal once in front."""
    import sys
    from sim import env, card
    port = kd.free_port()
    argv = [str(exe), "run", "--firmware", str(tmp_path / "m.bin"), "--elf", str(tmp_path / "a.elf"),
            "--port", str(port), "--no-open", "--sd", str(card(tmp_path))]
    r = await asyncio.to_thread(subprocess.run, [sys.executable, "-c", _BACKGROUND, *argv],
                                env=env(tmp_path / "cache"), capture_output=True, text=True, timeout=30)
    stat = r.stdout.strip()
    assert stat and "T" not in stat, f"kode-sim run was stopped in the background (ps: {stat!r}) {r.stderr}"
