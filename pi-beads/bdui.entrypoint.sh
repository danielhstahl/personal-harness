#!/bin/bash
set -e
bdui start
# keep the container's PID 1 alive so the daemon child doesn't get torn down with it
exec sleep infinity
