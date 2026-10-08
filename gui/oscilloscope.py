#!/usr/bin/env python3
"""STM32F3 Oscilloscope Frontend - GTK3 + matplotlib"""

import struct
import threading
import time
import glob
import os
import sys

import gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk, Gdk, GLib

import matplotlib
matplotlib.use("GTK3Agg")
import matplotlib.pyplot as plt
from matplotlib.figure import Figure
from matplotlib.backends.backend_gtk3agg import FigureCanvasGTK3Agg as FigureCanvas
import numpy as np

import serial
import serial.tools.list_ports
import subprocess
from pathlib import Path

# Protocol constants
DATA_SYNC = bytes([0xAA, 0x55])
CMD_SET_RATE = 0xA0
CMD_SET_CHANNEL = 0xB0
CMD_GET_MASK = 0xB2
CMD_ENTER_DFU = 0xD0
ACK_RATE = 0xA1
ACK_CHANNEL = 0xB1
ACK_MASK = 0xB3
ACK_DFU = 0xD1

SAMPLE_TIME_LABELS = [
    "1.5 cyc", "2.5 cyc", "4.5 cyc", "7.5 cyc",
    "19.5 cyc", "61.5 cyc", "181.5 cyc", "601.5 cyc",
]

PIN_NAMES = [
    "PA0", "PA1", "PA2", "PA3", "PF4",
    "PC0", "PC1", "PC2", "PC3", "PF2",
]

CHANNEL_COLORS = [
    "#e6194b", "#3cb44b", "#4363d8", "#f58231", "#911eb4",
    "#42d4f4", "#f032e6", "#bfef45", "#fabed4", "#469990",
]

MAX_SAMPLES = 2000


class SerialReader:
    def __init__(self):
        self.serial = None
        self.running = False
        self.thread = None
        self.callbacks = []

    def find_port(self):
        ports = serial.tools.list_ports.comports()
        for p in sorted(ports, key=lambda x: x.device):
            if "ttyACM" in p.device or "ttyUSB" in p.device:
                return p.device
        for pattern in ["/dev/ttyACM*", "/dev/ttyUSB*"]:
            matches = glob.glob(pattern)
            if matches:
                return sorted(matches)[0]
        return None

    def connect(self, port=None, baud=115200):
        if self.serial and self.serial.is_open:
            self.disconnect()
        if port is None:
            port = self.find_port()
        if port is None:
            return False, "No serial port found"
        try:
            self.serial = serial.Serial(port, baud, timeout=0.1)
            self.running = True
            self.thread = threading.Thread(target=self._read_loop, daemon=True)
            self.thread.start()
            return True, f"Connected to {port}"
        except Exception as e:
            return False, str(e)

    def disconnect(self):
        self.running = False
        if self.thread:
            self.thread.join(timeout=2)
        if self.serial and self.serial.is_open:
            self.serial.close()
        self.serial = None

    def send_command(self, cmd: bytes):
        if self.serial and self.serial.is_open:
            try:
                self.serial.write(cmd)
            except Exception:
                pass

    def send_set_rate(self, level: int):
        self.send_command(bytes([CMD_SET_RATE, level & 0x7F]))

    def send_set_channel(self, ch: int, enabled: bool, sample_time: int):
        self.send_command(bytes([CMD_SET_CHANNEL, ch & 0xFF, int(enabled), sample_time & 0x7F]))

    def send_get_mask(self):
        self.send_command(bytes([CMD_GET_MASK]))

    def send_enter_dfu(self):
        self.send_command(bytes([CMD_ENTER_DFU]))

    def on_data(self, callback):
        self.callbacks.append(callback)

    def _read_loop(self):
        buf = bytearray()
        while self.running and self.serial and self.serial.is_open:
            try:
                data = self.serial.read(256)
                if data:
                    buf.extend(data)
                    self._process_buf(buf)
            except Exception:
                time.sleep(0.05)

    def _process_buf(self, buf):
        while True:
            idx = self._find_sync(buf)
            if idx < 0:
                if len(buf) > 4:
                    del buf[:len(buf) - 3]
                break
            if idx > 0:
                del buf[:idx]
            if len(buf) < 3:
                break
            count = buf[2]
            pkt_len = 3 + count * 2
            if len(buf) < pkt_len:
                break
            samples = []
            for i in range(count):
                lo = buf[3 + i * 2]
                hi = buf[4 + i * 2]
                samples.append(lo | (hi << 8))
            ack_type = None
            if len(buf) > pkt_len:
                b = buf[pkt_len]
                if b == ACK_RATE:
                    ack_type = ("rate", buf[pkt_len + 1] if len(buf) > pkt_len + 1 else 0)
                elif b == ACK_CHANNEL:
                    if len(buf) > pkt_len + 3:
                        ack_type = ("channel", buf[pkt_len+1], buf[pkt_len+2], buf[pkt_len+3])
                elif b == ACK_MASK:
                    if len(buf) > pkt_len + 2:
                        ack_type = ("mask", buf[pkt_len+1], buf[pkt_len+2])
            del buf[:pkt_len]
            for cb in self.callbacks:
                try:
                    cb(samples, ack_type)
                except Exception:
                    pass

    def _find_sync(self, buf):
        for i in range(len(buf) - 1):
            if buf[i] == 0xAA and buf[i + 1] == 0x55:
                return i
        return -1


class OscilloscopeWidget(Gtk.Box):
    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)

        self.fig = Figure(figsize=(12, 6), dpi=100)
        self.fig.set_facecolor("#1a1a2e")
        self.canvas = FigureCanvas(self.fig)
        self.canvas.set_size_request(800, 400)
        self.pack_start(self.canvas, True, True, 0)

        self.ax = self.fig.add_subplot(111)
        self.ax.set_facecolor("#16213e")
        self.ax.tick_params(colors="#aaa", labelsize=8)
        for spine in self.ax.spines.values():
            spine.set_color("#444")
        self.ax.set_xlabel("Sample", color="#aaa", fontsize=9)
        self.ax.set_ylabel("ADC Value (12-bit)", color="#aaa", fontsize=9)
        self.ax.set_title("Oscilloscope", color="#eee", fontsize=11)
        self.ax.grid(True, alpha=0.2, color="#555")

        self.lines = []
        self.data_buffers = [[] for _ in range(10)]
        self.visible_channels = list(range(10))

        self.fig.tight_layout(pad=2)

    def update_channels(self, visible):
        self.visible_channels = visible
        for i in range(10):
            self.data_buffers[i] = self.data_buffers[i][-MAX_SAMPLES:]

    def append_samples(self, samples):
        n = len(samples)
        if n == 10:
            for ch in range(10):
                self.data_buffers[ch].append(samples[ch])
                if len(self.data_buffers[ch]) > MAX_SAMPLES:
                    self.data_buffers[ch] = self.data_buffers[ch][-MAX_SAMPLES:]
        elif n > 0:
            for i, ch in enumerate(self.visible_channels[:n]):
                if ch < 10:
                    self.data_buffers[ch].append(samples[i])
                    if len(self.data_buffers[ch]) > MAX_SAMPLES:
                        self.data_buffers[ch] = self.data_buffers[ch][-MAX_SAMPLES:]

    def redraw(self):
        self.ax.clear()
        self.ax.set_facecolor("#16213e")
        self.ax.tick_params(colors="#aaa", labelsize=8)
        for spine in self.ax.spines.values():
            spine.set_color("#444")
        self.ax.set_xlabel("Sample", color="#aaa", fontsize=9)
        self.ax.set_ylabel("ADC Value (12-bit)", color="#aaa", fontsize=9)
        self.ax.set_title("Oscilloscope", color="#eee", fontsize=11)
        self.ax.grid(True, alpha=0.2, color="#555")

        has_data = False
        for ch in self.visible_channels:
            data = self.data_buffers[ch]
            if len(data) > 1:
                self.ax.plot(data, color=CHANNEL_COLORS[ch], linewidth=1.0,
                             label=PIN_NAMES[ch], alpha=0.9)
                has_data = True

        if has_data:
            self.ax.legend(loc="upper right", fontsize=7,
                           facecolor="#1a1a2e", edgecolor="#444",
                           labelcolor="#ccc")

        self.ax.set_xlim(0, max(MAX_SAMPLES, 100))
        self.ax.set_ylim(0, 4096)
        self.fig.tight_layout(pad=2)
        self.canvas.draw_idle()


class ChannelRow(Gtk.Box):
    def __init__(self, ch_index, pin_name, on_change):
        super().__init__(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        self.ch_index = ch_index
        self.on_change = on_change

        self.check = Gtk.CheckButton()
        self.check.set_active(True)
        self.check.connect("toggled", self._on_toggle)
        self.pack_start(self.check, False, False, 0)

        color = Gdk.RGBA()
        color.parse(CHANNEL_COLORS[ch_index])
        self.color_label = Gtk.Label(label=f"  {pin_name}  ")
        self.color_label.override_background_color(Gtk.StateFlags.NORMAL, color)
        self.pack_start(self.color_label, False, False, 0)

        label = Gtk.Label(label=f"CH{ch_index} ({pin_name})")
        label.set_width_chars(14)
        self.pack_start(label, False, False, 0)

        self.st_combo = Gtk.ComboBoxText()
        for i, name in enumerate(SAMPLE_TIME_LABELS):
            self.st_combo.append(str(i), name)
        self.st_combo.set_active_id("2")
        self.st_combo.connect("changed", self._on_st_change)
        self.pack_start(self.st_combo, False, False, 0)

    def _on_toggle(self, widget):
        self.on_change()

    def _on_st_change(self, widget):
        self.on_change()

    def is_enabled(self):
        return self.check.get_active()

    def get_sample_time(self):
        return int(self.st_combo.get_active_id() or "2")


class MainWindow(Gtk.Window):
    def __init__(self):
        super().__init__(title="STM32F3 Oscilloscope")
        self.set_default_size(1100, 700)
        self.connect("destroy", self._on_destroy)

        self.reader = SerialReader()
        self.reader.on_data(self._on_serial_data)

        self.rate_level = 2
        self.update_pending = False

        main_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=0)
        self.add(main_box)

        left_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        left_box.set_margin_start(6)
        left_box.set_margin_top(6)
        left_box.set_margin_bottom(6)
        main_box.pack_start(left_box, True, True, 0)

        right_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        right_box.set_margin_end(6)
        right_box.set_margin_top(6)
        right_box.set_margin_bottom(6)
        right_box.set_size_request(320, -1)
        main_box.pack_end(right_box, False, False, 0)

        toolbar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        left_box.pack_start(toolbar, False, False, 0)

        self.port_combo = Gtk.ComboBoxText()
        self._refresh_ports()
        toolbar.pack_start(self.port_combo, False, False, 0)

        refresh_btn = Gtk.Button(label="Refresh")
        refresh_btn.connect("clicked", lambda _: self._refresh_ports())
        toolbar.pack_start(refresh_btn, False, False, 0)

        self.connect_btn = Gtk.Button(label="Connect")
        self.connect_btn.connect("clicked", self._on_connect)
        toolbar.pack_start(self.connect_btn, False, False, 0)

        update_btn = Gtk.Button(label="Update Firmware")
        update_btn.connect("clicked", self._on_update_firmware)
        toolbar.pack_start(update_btn, False, False, 0)       

        self.status_label = Gtk.Label(label="Disconnected")
        self.status_label.set_halign(Gtk.Align.START)
        toolbar.pack_start(self.status_label, True, True, 0)

        self.scope = OscilloscopeWidget()
        left_box.pack_start(self.scope, True, True, 0)

        rate_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        right_box.pack_start(rate_box, False, False, 0)

        rate_label = Gtk.Label(label="Sample Rate:")
        rate_box.pack_start(rate_label, False, False, 0)

        self.rate_combo = Gtk.ComboBoxText()
        for i, name in enumerate(SAMPLE_TIME_LABELS):
            self.rate_combo.append(str(i), name)
        self.rate_combo.set_active_id("2")
        self.rate_combo.connect("changed", self._on_rate_change)
        rate_box.pack_start(self.rate_combo, True, True, 0)

        ch_label = Gtk.Label(label="<b>Channels</b>")
        ch_label.set_use_markup(True)
        ch_label.set_halign(Gtk.Align.START)
        right_box.pack_start(ch_label, False, False, 0)

        ch_scroll = Gtk.ScrolledWindow()
        ch_scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        right_box.pack_start(ch_scroll, True, True, 0)

        ch_list = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        ch_scroll.add(ch_list)

        self.channel_rows = []
        for i in range(10):
            row = ChannelRow(i, PIN_NAMES[i], self._on_channel_change)
            ch_list.pack_start(row, False, False, 0)
            self.channel_rows.append(row)

        sep = Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL)
        right_box.pack_start(sep, False, False, 4)

        stats_label = Gtk.Label(label="<b>Statistics</b>")
        stats_label.set_use_markup(True)
        stats_label.set_halign(Gtk.Align.START)
        right_box.pack_start(stats_label, False, False, 0)

        self.stats_text = Gtk.Label(label="No data")
        self.stats_text.set_halign(Gtk.Align.START)
        self.stats_text.set_line_wrap(True)
        right_box.pack_start(self.stats_text, False, False, 0)

        self.pps_label = Gtk.Label(label="Packets/sec: 0")
        self.pps_label.set_halign(Gtk.Align.START)
        right_box.pack_start(self.pps_label, False, False, 0)

        self._packet_count = 0
        self._last_pps_time = time.time()

        GLib.timeout_add(50, self._update_display)

    def _refresh_ports(self):
        self.port_combo.remove_all()
        ports = serial.tools.list_ports.comports()
        found = set()
        for p in sorted(ports, key=lambda x: x.device):
            if "ttyACM" in p.device or "ttyUSB" in p.device:
                self.port_combo.append(p.device, f"{p.device} - {p.description}")
                found.add(p.device)
        for pattern in ["/dev/ttyACM*", "/dev/ttyUSB*"]:
            for dev in sorted(glob.glob(pattern)):
                if dev not in found:
                    self.port_combo.append(dev, dev)
        auto = self.reader.find_port()
        if auto:
            self.port_combo.set_active_id(auto)

    def _on_connect(self, btn):
        if self.reader.serial and self.reader.serial.is_open:
            self.reader.disconnect()
            self.connect_btn.set_label("Connect")
            self.status_label.set_text("Disconnected")
        else:
            port = self.port_combo.get_active_id()
            if not port:
                port = self.reader.find_port()
            ok, msg = self.reader.connect(port)
            if ok:
                self.connect_btn.set_label("Disconnect")
                self.status_label.set_text(msg)
                self.status_label.override_color(Gtk.StateFlags.NORMAL, Gdk.RGBA(parse="#4caf50"))
                self.reader.send_get_mask()
                self._on_channel_change()
            else:
                self.status_label.set_text(f"Error: {msg}")
                self.status_label.override_color(Gtk.StateFlags.NORMAL, Gdk.RGBA(parse="#f44336"))

    def _on_update_firmware(self, btn):
        if not self.reader.serial or not self.reader.serial.is_open:
            dialog = Gtk.MessageDialog(
                transient_for=self,
                flags=0,
                message_type=Gtk.MessageType.WARNING,
                buttons=Gtk.ButtonsType.OK,
                text="Please connect to device first"
            )
            dialog.run()
            dialog.destroy()
            return
        
        self.reader.send_enter_dfu()
        
        dialog = Gtk.MessageDialog(
            transient_for=self,
            flags=0,
            message_type=Gtk.MessageType.INFO,
            buttons=Gtk.ButtonsType.NONE,
            text="Device entering DFU mode...\nPlease wait."
        )
        dialog.show_all()
        
        self.reader.disconnect()
        self.connect_btn.set_label("Connect")
        self.status_label.set_text("Disconnected")
        
        time.sleep(2)
        
        try:
            result = subprocess.run(
                ["./flash.sh"],
                capture_output=True,
                text=True,
                timeout=30
            )
            
            dialog.destroy()
            
            if result.returncode == 0:
                success_dialog = Gtk.MessageDialog(
                    transient_for=self,
                    flags=0,
                    message_type=Gtk.MessageType.INFO,
                    buttons=Gtk.ButtonsType.OK,
                    text="Firmware updated successfully!"
                )
                success_dialog.run()
                success_dialog.destroy()
            else:
                error_dialog = Gtk.MessageDialog(
                    transient_for=self,
                    flags=0,
                    message_type=Gtk.MessageType.ERROR,
                    buttons=Gtk.ButtonsType.OK,
                    text=f"Flash failed:\n{result.stderr}"
                )
                error_dialog.run()
                error_dialog.destroy()
        except Exception as e:
            dialog.destroy()
            error_dialog = Gtk.MessageDialog(
                transient_for=self,
                flags=0,
                message_type=Gtk.MessageType.ERROR,
                buttons=Gtk.ButtonsType.OK,
                text=f"Error: {str(e)}"
            )
            error_dialog.run()
            error_dialog.destroy()

    def _on_rate_change(self, widget):
        self.rate_level = int(widget.get_active_id() or "2")
        self.reader.send_set_rate(self.rate_level)

    def _on_channel_change(self):
        for row in self.channel_rows:
            self.reader.send_set_channel(
                row.ch_index, row.is_enabled(), row.get_sample_time()
            )
        visible = [row.ch_index for row in self.channel_rows if row.is_enabled()]
        self.scope.update_channels(visible)

    def _on_serial_data(self, samples, ack_type):
        self.scope.append_samples(samples)
        self._packet_count += 1
        if ack_type:
            if ack_type[0] == "rate":
                pass
            elif ack_type[0] == "channel":
                pass
            elif ack_type[0] == "mask":
                ch_mask = ack_type[1] | (ack_type[2] << 8)
                for i, row in enumerate(self.channel_rows):
                    if i < 10:
                        enabled = bool(ch_mask & (1 << i))
                        row.check.set_active(enabled)

    def _update_display(self):
        now = time.time()
        elapsed = now - self._last_pps_time
        if elapsed >= 1.0:
            pps = int(self._packet_count / elapsed)
            self.pps_label.set_text(f"Packets/sec: {pps}")
            self._packet_count = 0
            self._last_pps_time = now

            visible = [r.ch_index for r in self.channel_rows if r.is_enabled()]
            parts = []
            for ch in visible:
                data = self.scope.data_buffers[ch]
                if data:
                    last = data[-1]
                    v = last * 3.3 / 4096
                    parts.append(f"{PIN_NAMES[ch]}: {last} ({v:.2f}V)")
            if parts:
                self.stats_text.set_text("\n".join(parts))
            else:
                self.stats_text.set_text("No data")

        self.scope.redraw()
        return True

    def _on_destroy(self, widget):
        self.reader.disconnect()
        Gtk.main_quit()


def main():
    win = MainWindow()
    win.show_all()
    Gtk.main()


if __name__ == "__main__":
    main()
