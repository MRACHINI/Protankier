class CaptchaModelServer_checkCaptcha {
  constructor({ captchaSolution, location, captchaPrefix, spaceId = 100072911n }) {
    this.spaceId = BigInt(spaceId);
    this.methodId = 3312562673813486434n;
    this.captchaSolution = captchaSolution ?? "";
    this.location = location !== undefined ? location : captchaPrefix !== undefined ? captchaPrefix : 6;
  }

  serialize() {
    const tokenBytes = Buffer.from(this.captchaSolution, "utf8");

    // Tanki strings use the Alternativa length format: a single byte for
    // lengths below 128, otherwise two big-endian bytes with the high bit of
    // the first byte set (max 32767). reCAPTCHA tokens are ~900 chars, so the
    // old hardcoded 0x83 prefix only worked for one specific token length.
    if (tokenBytes.length > 32767) {
      throw new RangeError("Captcha solution is too long for the Tanki string codec");
    }

    const lengthPrefix =
      tokenBytes.length < 128
        ? [tokenBytes.length]
        : [0x80 | (tokenBytes.length >> 8), tokenBytes.length & 0xff];

    const buffer_length = 2 + 1 + 8 + 8 + 4 + lengthPrefix.length + tokenBytes.length;
    const buffer = new ArrayBuffer(buffer_length);
    const view = new DataView(buffer);
    let pos = 0;
    view.setUint16(pos, buffer_length - 2); // Write the packet size
    pos += 2; // Shift position by 2 bytes since the packet size is a short
    view.setUint8(pos, 0); // Nullmap byte
    pos++;
    view.setBigInt64(pos, this.spaceId); // Space ID
    pos += 8;
    view.setBigInt64(pos, this.methodId); // Method ID
    pos += 8;
    view.setUint32(pos, this.location); // Captcha location enum
    pos += 4;

    for (const byte of lengthPrefix) {
      view.setUint8(pos, byte);
      pos++;
    }

    for (const byte of tokenBytes) {
      view.setUint8(pos, byte);
      pos++;
    }

    return buffer;
  }
}

export default CaptchaModelServer_checkCaptcha;
