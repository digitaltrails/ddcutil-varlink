import json
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


def handle_event(event):
    """Process individual events with beautiful, readable dot-notation."""
    # The event object matches the IDL: it has an 'event' property containing the structure
    ev_data = event.event
    print(ev_data)

    if ev_data.kind == "service_initialized":
        print("🚀 Service initialized and ready.")

    elif ev_data.kind == "vcp_changed":
        # Decode the embedded JSON string inside the data field
        # The prompt notes 'data' is a string containing the actual variant payload
        details = json.loads(ev_data.data, object_hook=lambda d: SimpleNamespace(**d))

        print(f"🔄 VCP Changed on Display #{details.display_number}:")
        print(f"   Feature Code: {details.vcp_code}")
        print(f"   New Value   : {details.new_value}")

    elif ev_data.kind == "connected_displays_changed":
        print(f"{ev_data=}")
        details = json.loads(ev_data.data, object_hook=lambda d: SimpleNamespace(**d))
        print(f"{details=}")
        print(f"   Event Type: {details.event_type}")
        print(f"   New Value : {details.flags}")


    elif ev_data.kind == "stream_closed":
        print("🛑 Server requested the event stream to close.")
        return False

    return True


def main():
    service_address = f"unix:/run/user/{os.getuid()}/ddcutil-varlink.socket"
    service_name = "com.ddcutil.DdcutilInterface"

    print(f"📡 Subscribing to event stream at: {service_address}")

    try:
        with Client(service_address) as client:
            with client.open(service_name) as ddcutil:

                # CRITICAL STEP: Use _more=True to turn this method call into a generator stream
                event_stream = ddcutil.Subscribe(use_polling=True, _more=True)

                # Loop blocks and waits for new events from the socket
                for raw_event in event_stream:
                    event_namespace = to_namespace(raw_event)

                    # Process the namespace cleanly
                    keep_running = handle_event(event_namespace)

                    if not keep_running:
                        break

    except VarlinkError as e:
        print(f"❌ Varlink error in stream: {e.error()}")
    except ConnectionRefusedError:
        print("❌ Error: The Varlink background daemon is not running.")
    except KeyboardInterrupt:
        print("\n👋 Unsubscribed cleanly via user interrupt.")


if __name__ == "__main__":
    main()
