#!/bin/bash
set -e
sudo install -m 755 "$HOME/logsentry/target/release/logsentry-gui" /usr/local/bin/logsentry-gui
echo "installiert."
