<!-- 
SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
SPDX-License-Identifier: GPL-2.0-or-later
-->
A ddcutil varlink service for control of DDC Monitors/VDUs

> [!WARNING]
> When using this service, avoid excessively writing VCP values because each VDU's NVRAM likely has a write-cycle limit/lifespan. The suggested guideline is to limit updates to rates comparable to those observed when using the VDU's onboard controls. Avoid coding that might rapidly or infinitely loop, including when recovering from errors and bugs.
>
> Non-standard manufacturer specific features should only be experimented with caution, some may have irreversible consequences, including bricking the hardware.

> [!IMPORTANT]
> This software is still in development. 
> The service implementation is incomplete. The varlink interface may change.
> Currently implemented methods:
> - Detect
> - ListDetected
> - GetVcp
> - GetMultipleVcp
> - SetVcp
> - Subscribe
> - GetDdcutilVersion
> - GetAttributesReturnedByDetect

The aim of this service is to make it easier to create highly-responsive widgets 
and apps for [ddcutil](https://www.ddcutil.com/).   The service is based on [ddcutil-service](https://github.com/digitaltrails/ddcutil-service), a 
similar D-Bus service.

The service is written in Rust.   Compared to other implementations of similar 
services, the code for `ddcutil-varlink` is quite compact and the abstractions 
are relatively shallow. Providing you know Rust and a little about [varlink](https://varlink.org/), the code 
should be quite easy to follow.  

Once built, running the executable should make a ddcutil varlink service available
on `unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket`.  The service runs under
a user account (assuming libddcutil is installed with required permissions).
Any type of varlink client can be used to interact with 
the service. For example, from the command line you could use the 
systemd `varlinkctl` command:

```
SERVICE="unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket"
INTERFACE="com.ddcutil.DdcutilInterface"
varlinkctl list-methods $SERVICE
varlinkctl introspect $SERVICE
varlinkctl call $SERVICE "${INTERFACE}.Detect" '{"flags":0}'
varlinkctl call $SERVICE "${INTERFACE}.GetVcp '{"display_number":1,"edid_base64":"","vcp_code":16,"flags":0}'
varlinkctl call $SERVICE "${INTERFACE}.SetVcp '{"display_number":5,"edid_base64":"","vcp_code":16,"new_value":50,"flags":0}'
```

### Build and run

```aiignore
# Build and run a release version
cargo build --release
RUST_LOG=debug ./target/release/ddcutil-varlink

# Build and run for debugging
cargo build --debug
RUST_BACKTRACE=1 RUST_LOG=debug ./target/debug/ddcutil-varlink
```


### Installing the ddcutil-varlink as a systemd auto-started service

Create the following service files and amend the `ExecStart` location. 

```aiignore
# systemd/user/ddcutil.service                                                                                                                      ✔  10664  09:32:53
[Unit]
Description=ddcutil Varlink Service
Requires=ddcutil.socket
After=ddcutil.socket

[Service]
Type=simple
ExecStart=/usr/bin/ddcutil-varlink
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
```

```aiignore
# systemd/user/ddcutil.socket
[Unit]
Description=ddcutil Varlink Service Socket

[Socket]
ListenStream=%t/ddcutil-varlink.socket
SocketMode=0600

[Install]
WantedBy=sockets.target
```

> [!WARNING] 
> The following is as yet untested

Install the service for a single user:
```aiignore
# Reload the user systemd manager daemon
systemctl --user daemon-reload

# Enable and start the socket unit immediately
systemctl --user enable --now ddcutil.socket

# Verify running
systemctl --user status ddcutil.socket
```

#### Installation via prebuilt binaries 



### Optional ddcutil-varlink-client

### Acknowledgements

Thanks go out to Sanford Rockowitz ([rockowitz](https://github.com/rockowitz)) 
for [libddcutil, ddcutil](https://www.ddcutil.com/).


