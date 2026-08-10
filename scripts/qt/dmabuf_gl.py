#!/usr/bin/env python3
"""Minimal Qt6/PySide6 test client, QOpenGLWidget -- a real GL context,
exercising the same class of GPU-backed buffer as scripts/gtk/dmabuf_gl.py
(qtwayland's own EGL/dmabuf integration is a different code path than
GTK's, not just a relabeling of it -- see scripts/qt/common.py's own doc
comment for why that difference is the whole point of this client
existing). wl_buffer (dmabuf or otherwise) is deliberately outside the
recreation graph today (see recreation.rs), so a client that keeps
reusing its own pre-crash GPU buffer after a reconnect is expected to
still hit that gap with this script -- consistent with the GTK
counterpart's own doc comment.

Run with: WAYLAND_DISPLAY=wayland-0 QT_QPA_PLATFORM=wayland python3 scripts/qt/dmabuf_gl.py
"""

import math
import sys
import time

from PySide6.QtCore import Qt, QTimer
from PySide6.QtGui import QColor, QPainter, QSurfaceFormat
from PySide6.QtOpenGLWidgets import QOpenGLWidget
from PySide6.QtWidgets import QApplication

import common

LOG_EVERY_N_FRAMES = 30  # roughly once a second at the ~30fps timer below


class TestWindow(QOpenGLWidget):
    def __init__(self, app):
        super().__init__()
        self._app = app
        self.setWindowTitle("wl-res test: dmabuf (GL)")
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
            ctx = self.context()
            fmt = ctx.format() if ctx else None
            common.log.info(
                "realized: qpa_platform=%s widget=%s gl_profile=%s gl_version=%s",
                self._app.platformName(),
                type(self).__name__,
                fmt.profile().name if fmt else "<none>",
                f"{fmt.majorVersion()}.{fmt.minorVersion()}" if fmt else "<none>",
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

    def paintGL(self):
        # Simple animated content via QPainter over the GL-backed surface
        # (Qt's OpenGL painter engine handles this) -- same visual shape
        # as basic_shm.py's own, just GPU-backed underneath.
        t = time.monotonic() - self._start_time
        width, height = self.width(), self.height()
        x = (width / 2) * (1 + 0.8 * math.sin(t))
        painter = QPainter(self)
        painter.fillRect(self.rect(), QColor.fromRgbF(0.15, 0.15, 0.2))
        painter.setPen(Qt.NoPen)
        painter.setBrush(QColor.fromRgbF(0.2, 0.5, 0.9))
        radius = min(width, height) / 8
        painter.drawEllipse(int(x - radius), int(height / 2 - radius), int(radius * 2), int(radius * 2))
        painter.end()

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

    # Explicit format, requested BEFORE the widget's native surface is
    # created -- same ordering constraint scripts/gtk/dmabuf_gl.py's own
    # GSK_RENDERER-before-Gtk-import has, just Qt's own version of it.
    fmt = QSurfaceFormat()
    fmt.setRenderableType(QSurfaceFormat.OpenGL)
    QSurfaceFormat.setDefaultFormat(fmt)

    window = TestWindow(app)
    window.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
