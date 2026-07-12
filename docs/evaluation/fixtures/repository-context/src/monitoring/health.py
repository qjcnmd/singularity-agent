def health_payload(component, ready=True):
    return {"component": component, "ready": bool(ready)}
