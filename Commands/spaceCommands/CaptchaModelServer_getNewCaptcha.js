class CaptchaModelServer_getNewCaptcha {
    constructor({ spaceId = 100072911n, location = 6, captchaPrefix } = {}) {
        this.spaceId = BigInt(spaceId);
        this.methodId = 7369561336714029596n;
        this.location = captchaPrefix !== undefined ? captchaPrefix : location;
    }

    serialize() {
        const parameters = {
            spaceId: this.spaceId,
            methodId: this.methodId,
        };
        const values = Object.values(parameters);
        const buffer_length = 23;
        const buffer = new ArrayBuffer(buffer_length);
        const view = new DataView(buffer);
        let pos = 0;
        view.setUint16(pos, buffer_length - 2); // Write the packet size
        pos += 2; // Shift position by 2 bytes since the packet size is a short
        view.setUint8(pos, 0); // Nullmap byte
        pos++
        view.setBigInt64(pos, values[0]); // Space ID
        pos+=8;
        view.setBigInt64(pos, values[1]); // Method ID
        pos+=8;
        view.setUint32(pos, this.location); // Captcha location enum

        return buffer;
    }
}

export default CaptchaModelServer_getNewCaptcha;
