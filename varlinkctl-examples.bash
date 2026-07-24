# SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
# SPDX-License-Identifier: GPL-2.0-or-later

SERVICE="unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket"

varlinkctl list-methods $SERVICE
varlinkctl introspect $SERVICE

varlinkctl call $SERVICE com.ddcutil.DdcutilInterface.Detect '{"include_offline": false}'
varlinkctl call $SERVICE com.ddcutil.DdcutilInterface.GetVcp '{"display_number":1,"vcp_code":16'}'
varlinkctl call $SERVICE com.ddcutil.DdcutilInterface.Subscribe '{}'
varlinkctl call $SERVICE com.ddcutil.DdcutilInterface.GetMultipleVcp '{"display_number":1,"vcp_codes":[16,20]}'
varlinkctl --more --timeout=infinity  call $SERVICE com.ddcutil.DdcutilInterface.Subscribe '{}'
varlinkctl call $SERVICE com.ddcutil.DdcutilInterface.GetMultipleVcp '{"edid_base64":"AP///////wAi8GkoAQEBAQgUAQSlNiN4Lvy","vcp_codes":[16,20], "options": { "allow_edid_prefix": true  } }'