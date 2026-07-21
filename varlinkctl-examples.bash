# SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
# SPDX-License-Identifier: GPL-2.0-or-later
varlinkctl call unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket com.ddcutil.DdcutilInterface.Detect '{"flags":0}'
varlinkctl call unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket com.ddcutil.DdcutilInterface.GetVcp '{"display_number":1,"edid_base64":"","vcp_code":16,"flags":0}'
varlinkctl call unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket com.ddcutil.DdcutilInterface.Subscribe '{}'
varlinkctl call unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket com.ddcutil.DdcutilInterface.GetMultipleVcp '{"display_number":1,"edid_base64":"","vcp_codes":[16,20],"flags":0}'
varlinkctl call unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket com.ddcutil.DdcutilInterface.GetMultipleVcp '{"display_number":-1,"edid_base64":"AP///////wAi8GkoAQEBAQgUAQSlNiN4Lvy","vcp_codes":[16,20],"flags":1}'
