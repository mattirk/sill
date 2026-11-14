#!/usr/bin/python3
"""A small X11 window ("sill-witness") that writes every key and button it receives to a file.

    x11_input_witness.py LOG

Used by the host scenario on a headless Weston/Xwayland display to prove that Host control's
display input reaches a window. Needs python-xlib (the system Python's). Lines in LOG:
`mapped X Y W H`, `button N X Y`, `key NAME`.
"""
import sys

from Xlib import X, XK, display

log = open(sys.argv[1], "w", buffering=1)
d = display.Display()
screen = d.screen()
window = screen.root.create_window(
    40, 30, 300, 200, 0, screen.root_depth, X.InputOutput, X.CopyFromParent,
    background_pixel=screen.white_pixel,
    event_mask=X.KeyPressMask | X.ButtonPressMask | X.StructureNotifyMask | X.ExposureMask)
window.set_wm_name("sill-witness")
window.set_wm_class("sill-witness", "sill-witness")
window.map()
d.sync()
while True:
    event = d.next_event()
    if event.type == X.MapNotify or event.type == X.ConfigureNotify or event.type == X.Expose:
        geometry = window.get_geometry()
        at = screen.root.translate_coords(window, 0, 0)
        log.write(f"mapped {at.x} {at.y} {geometry.width} {geometry.height}\n")
        window.set_input_focus(X.RevertToParent, X.CurrentTime)
        d.sync()
    elif event.type == X.ButtonPress:
        log.write(f"button {event.detail} {event.root_x} {event.root_y}\n")
    elif event.type == X.KeyPress:
        keysym = d.keycode_to_keysym(event.detail, 1 if event.state & X.ShiftMask else 0)
        log.write(f"key {XK.keysym_to_string(keysym) or keysym}\n")
