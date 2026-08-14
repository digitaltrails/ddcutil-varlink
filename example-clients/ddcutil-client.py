from varlink import Client
import os
from types import SimpleNamespace


def to_namespace(data):
    """Recursively converts Varlink objects, dictionaries, and lists into SimpleNamespaces."""
    # 1. Unpack Varlink's custom objects automatically if detected
    if hasattr(data, "as_dict") and callable(getattr(data, "as_dict")):
        data = data.as_dict()

    # 2. Recursively convert standard dictionaries
    if isinstance(data, dict):
        return SimpleNamespace(**{k: to_namespace(v) for k, v in data.items()})

    # 3. Recursively convert lists
    elif isinstance(data, list):
        return [to_namespace(i) for i in data]

    return data

def main():
    service_address = f'unix:/run/user/{os.getuid()}/ddcutil-varlink.socket'
    service_name = "com.ddcutil.DdcutilInterface"
    print(service_address)
    with Client(service_address) as client:
        with client.open(service_name) as ddcutil:
            detected_displays = to_namespace(ddcutil.Detect(include_offline=True))
            for display in detected_displays.displays:
                print(f"{display.display_number=} {display.model_name=}")
                display_number  = 1
                value = to_namespace(ddcutil.GetVcp(display_number=display_number, vcp_code=16))
                print(f"getvcp: {display_number=} brightness {value=}")
                print(f"getvcp: {display_number=} brightness {value.current=}")

if __name__ == '__main__':
    main()