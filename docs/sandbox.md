# Sandbox

Terminal execution is disabled until sandboxd is configured. The app has no host-shell tool and receives no container socket. sandboxd will own rootless runtime operations through a narrow authenticated protocol.

Default policy is restricted egress, unprivileged user, dropped capabilities, no host PID/IPC/network namespace, read-only base, explicit workspace volume, CPU/memory/PID limits, timeout cleanup, seccomp, and LSM compatibility. Cloud metadata, loopback, private ranges, runtime daemons, and host services are denied unless explicitly authorized.

Terminal metadata and output cursors persist. If infrastructure restart kills a process, its state becomes terminated; reconnect does not claim process survival.
