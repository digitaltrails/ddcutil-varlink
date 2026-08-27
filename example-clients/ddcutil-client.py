import os
from types import SimpleNamespace
from varlink import Client, VarlinkError


def to_namespace(data):
    """Recursively converts Varlink objects, dictionaries, and lists into SimpleNamespaces."""
    if hasattr(data, "as_dict") and callable(getattr(data, "as_dict")):
        data = data.as_dict()
    if isinstance(data, dict):
        return SimpleNamespace(**{k: to_namespace(v) for k, v in data.items()})
    elif isinstance(data, list):
        return [to_namespace(i) for i in data]
    return data


def main():
    service_address = f"unix:/run/user/{os.getuid()}/ddcutil-varlink.socket"
    service_name = "com.ddcutil.DdcutilInterface"

    print(f"Connecting to: {service_address}")

    try:
        with Client(service_address) as client:
            with client.open(service_name) as ddcutil:
                try:
                    raw_detect = ddcutil.Detect(include_offline=True)
                    detected_displays = to_namespace(raw_detect)
                except VarlinkError as e:
                    print(f"Failed to detect displays: {e.error()}")
                    return

                for display in detected_displays.displays:
                    print(f"\n{display.display_number=} {display.model_name=}")
                    display_number = display.display_number
                    try:
                        # Attempting to fetch a VCP code (e.g., 16 for brightness)
                        raw_vcp = ddcutil.GetVcp(
                            display_number=display_number, vcp_code=16
                        )
                        value = to_namespace(raw_vcp)

                        print(f"getvcp: {display_number=} brightness {value=}")
                        print(f"getvcp: {display_number=} brightness {value.current=}")
                    except VarlinkError as vcp_err:
                        # Convert error details to namespace if they exist
                        error_details = to_namespace(vcp_err.parameters())
                        print(f"Cannot read VCP 16 on display {display_number}: {vcp_err.error()}")
                        print(f"   Details from server: {error_details}")

    except ConnectionRefusedError:
        print("Error: Varlink server is not running or the socket path is incorrect.")
    except FileNotFoundError:
        print(f"Error: Socket file path '{service_address}' does not exist.")


if __name__ == "__main__":
    main()
