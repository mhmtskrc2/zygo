# The warm function behind POST /lower. Loaded once by `zygo up`;
# every request runs in a fresh copy of this process.

def handler(event):
    return {"text": event["text"].lower()}
