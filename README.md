# wol-mqtt

wol-mqtt will connect to a MQTT broker at interact with it in two ways:

1. It will send either "UP" or "DOWN" to "{topic_base}/status/{device}" if the online status of a device changes.
2. It will listen on "{topic_base}/command/{device}" and send a Wake-On-Lan packet to the specified device if a message is received.

See [`example-config.yaml`](./example-config.yaml) for an example configuration.
