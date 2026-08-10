#!/usr/bin/env python3
"""Minimal Qt6/PySide6 test client, plain QWidget + QPainter -- raster
(software/SHM) rendering, the Qt counterpart to scripts/gtk/basic_shm.py.
See scripts/qt/common.py's own doc comment for why this exists alongside
the GTK clients, and for the stall-detection caveat.

Run with: WAYLAND_DISPLAY=wayland-0 QT_QPA_PLATFORM=wayland python3 scripts/qt/basic_shm.py
"""

import math
import sys
import time

from PySide6.QtCore import Qt, QTimer
from PySide6.QtGui import QColor, QPainter
from PySide6.QtWidgets import QApplication, QWidget

import common

LOG_EVERY_N_FRAMES = 30  # roughly once a second at the ~30fps timer below


class TestWindow(QWidget):
    def __init__(self, app):
        super().__init__()
        self._app = app
        self.setWindowTitle("wl-res test: basic (SHM/raster)")
        self.resize(320, 240)
        self._frame_count = 0
        self._start_time = time.monotonic()
        self._logged_platform = False
        self._stall = common.StallTracker(on_give_up=app.quit)

        self._redraw_timer = QTimer(self)
        self._redraw_timer.timeout.connect(self._on_redraw)
        self._redraw_timer.start(33)  # ~30fps

        self._stall_timer = QTimer(self)
        self._stall_timer.timeout.connect(self._stall.check_stall)
        self._stall_timer.start(1000)

    def showEvent(self, event):
        super().showEvent(event)
        if not self._logged_platform:
            self._logged_platform = True
            common.log.info(
                "realized: qpa_platform=%s widget=%s", self._app.platformName(), type(self).__name__
            )
        common.log.info("window mapped")

    def hideEvent(self, event):
        super().hideEvent(event)
        common.log.info("window unmapped")

    def _on_redraw(self):
        self._stall.tick()
        self._frame_count += 1
        if self._frame_count % LOG_EVERY_N_FRAMES == 0:
            elapsed = time.monotonic() - self._start_time
            common.log.info("frame tick #%d (t+%.1fs)", self._frame_count, elapsed)
        self.update()

    def paintEvent(self, event):
        # Simple animated content -- a circle sweeping left to right --
        # just enough to force genuinely new buffer contents each frame,
        # same reasoning as the GTK clients' own _on_draw.
        t = time.monotonic() - self._start_time
        width, height = self.width(), self.height()
        x = (width / 2) * (1 + 0.8 * math.sin(t))
        painter = QPainter(self)
        painter.fillRect(self.rect(), QColor.fromRgbF(0.15, 0.15, 0.15))
        painter.setPen(Qt.NoPen)
        painter.setBrush(QColor.fromRgbF(0.9, 0.3, 0.2))
        radius = min(width, height) / 8
        painter.drawEllipse(int(x - radius), int(height / 2 - radius), int(radius * 2), int(radius * 2))

    def closeEvent(self, event):
        common.log.info("close requested, total frames rendered: %d", self._frame_count)
        super().closeEvent(event)

    def keyPressEvent(self, event):
        if event.key() == Qt.Key_Escape:
            common.log.info("Escape pressed, closing")
            self.close()
        else:
            super().keyPressEvent(event)


def main():
    common.log_startup_environment()
    app = QApplication(sys.argv)
    window = TestWindow(app)
    window.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
