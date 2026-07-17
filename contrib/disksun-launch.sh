#!/bin/sh
# Launch disksun (sunburst disk-usage GUI); no args = whole-disk view.
# setsid detaches it from waybar so a bar reload doesn't kill it.
exec setsid disksun </dev/null >/dev/null 2>&1
