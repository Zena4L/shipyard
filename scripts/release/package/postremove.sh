#!/bin/sh
# After the .deb or .rpm is removed or upgraded (step 15.5b): systemd told
# its units went or changed. The AppArmor profile was the pre-removal
# script's. Never fails.
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload 2>/dev/null || true
fi
exit 0
