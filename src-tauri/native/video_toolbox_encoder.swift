import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

public typealias GoLiveEncodedCallback = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafePointer<UInt8>?,
    Int,
    Int64,
    Int32,
    UInt8
) -> Void

public typealias GoLiveEncoderErrorCallback = @convention(c) (
    UnsafeMutableRawPointer?,
    Int32
) -> Void

private final class GoLiveEncoder {
    let callback: GoLiveEncodedCallback
    let errorCallback: GoLiveEncoderErrorCallback
    let callbackContext: UnsafeMutableRawPointer?
    var session: VTCompressionSession?
    var forceNextKeyframe = false
    var closed = false

    init(
        width: Int32,
        height: Int32,
        bitrate: Int32,
        frameRate: Int32,
        _ codec: Int32,
        callback: GoLiveEncodedCallback,
        errorCallback: GoLiveEncoderErrorCallback,
        callbackContext: UnsafeMutableRawPointer?
    ) throws {
        self.callback = callback
        self.errorCallback = errorCallback
        self.callbackContext = callbackContext

        // goDrinking product codec: H.264 Constrained Baseline only (SDP
        // 42e02a, packetization-mode 1). Non-zero codec flags (HEVC/High/AV1)
        // are rejected by the Rust seam before this point; fail loudly here
        // rather than emitting a bitstream no Viewer can decode.
        guard codec == 0 else { throw GoLiveEncoderError.unavailable }
        let codecType: CMVideoCodecType = kCMVideoCodecType_H264
        var createdSession: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            width: width,
            height: height,
            codecType: codecType,
            encoderSpecification: nil,
            imageBufferAttributes: nil,
            compressedDataAllocator: nil,
            outputCallback: goLiveCompressionOutput,
            refcon: Unmanaged.passUnretained(self).toOpaque(),
            compressionSessionOut: &createdSession
        )
        guard status == noErr, let createdSession else {
            throw GoLiveEncoderError.status(status)
        }
        session = createdSession

        try setProperty(kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        // Constrained Baseline contract, identical to the MF/OpenH264 arms:
        // Baseline AutoLevel (level follows dimensions), no B-frames
        // (AllowFrameReordering=false below), CAVLC entropy coding (Baseline
        // forbids CABAC; set explicitly so a driver default can never drift),
        // GOP ~= 1s (MaxKeyFrameInterval=fps + duration 1.0), and BT.709
        // color (primaries/transfer/matrix). Input pixel buffers are 32BGRA
        // from ScreenCaptureKit; the 709 properties govern the conversion.
        try setProperty(
            kVTCompressionPropertyKey_ProfileLevel,
            value: kVTProfileLevel_H264_Baseline_AutoLevel
        )
        // Best-effort: Baseline already implies CAVLC, this only pins it.
        try? setProperty(
            kVTCompressionPropertyKey_H264EntropyMode,
            value: kVTH264EntropyMode_CAVLC
        )
        try setProperty(
            kVTCompressionPropertyKey_ColorPrimaries,
            value: kCMFormatDescriptionColorPrimaries_ITU_R_709_2
        )
        try setProperty(
            kVTCompressionPropertyKey_TransferFunction,
            value: kCMFormatDescriptionTransferFunction_ITU_R_709_2
        )
        try setProperty(
            kVTCompressionPropertyKey_YCbCrMatrix,
            value: kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2
        )
        try setProperty(
            kVTCompressionPropertyKey_AllowFrameReordering,
            value: kCFBooleanFalse
        )
        try setProperty(
            kVTCompressionPropertyKey_AverageBitRate,
            value: NSNumber(value: bitrate)
        )
        try setProperty(
            kVTCompressionPropertyKey_ExpectedFrameRate,
            value: NSNumber(value: frameRate)
        )
        try setProperty(
            kVTCompressionPropertyKey_DataRateLimits,
            value: NSArray(objects: NSNumber(value: bitrate / 8), NSNumber(value: 1))
        )
        // One-second keyframe interval keeps join latency low while Baseline
        // plus disabled frame reordering prevents B-frame latency.
        try setProperty(
            kVTCompressionPropertyKey_MaxKeyFrameInterval,
            value: NSNumber(value: frameRate)
        )
        try setProperty(
            kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
            value: NSNumber(value: 1.0)
        )
        let prepareStatus = VTCompressionSessionPrepareToEncodeFrames(createdSession)
        guard prepareStatus == noErr else {
            throw GoLiveEncoderError.status(prepareStatus)
        }
    }

    func close() {
        guard !closed else { return }
        closed = true
        if let session {
            // CompleteFrames is the callback quiescence point. The C-ABI
            // destroy function does not release the Rust callback context
            // until this call has returned.
            VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
            VTCompressionSessionInvalidate(session)
            self.session = nil
        }
    }

    deinit {
        close()
    }

    func encode(pixelBuffer: CVPixelBuffer, pts: CMTime) throws {
        guard !closed, let session else { throw GoLiveEncoderError.unavailable }
        var flags = VTEncodeInfoFlags()
        var frameProperties: CFDictionary?
        if forceNextKeyframe {
            frameProperties = NSDictionary(
                object: kCFBooleanTrue as Any,
                forKey: NSString(string: kVTEncodeFrameOptionKey_ForceKeyFrame as String)
            )
            forceNextKeyframe = false
        }
        let status = VTCompressionSessionEncodeFrame(
            session,
            imageBuffer: pixelBuffer,
            presentationTimeStamp: pts,
            duration: .invalid,
            frameProperties: frameProperties,
            sourceFrameRefcon: nil,
            infoFlagsOut: &flags
        )
        guard status == noErr else { throw GoLiveEncoderError.status(status) }
    }

    func forceKeyframe() throws {
        guard !closed, session != nil else { throw GoLiveEncoderError.unavailable }
        forceNextKeyframe = true
    }

    func setBitrate(_ bitrate: Int32) throws {
        try setProperty(kVTCompressionPropertyKey_AverageBitRate, value: NSNumber(value: bitrate))
        // Keep the burst cap in sync: otherwise a live raise (preset change,
        // custom slider, REMB recovery) stays choked by the creation-time
        // limit and the picture blocks up despite the higher average.
        try setProperty(
            kVTCompressionPropertyKey_DataRateLimits,
            value: NSArray(objects: NSNumber(value: bitrate / 8), NSNumber(value: 1))
        )
    }

    func flush() throws {
        guard !closed, let session else { throw GoLiveEncoderError.unavailable }
        let status = VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        guard status == noErr else { throw GoLiveEncoderError.status(status) }
    }

    private func setProperty(_ key: CFString, value: CFTypeRef) throws {
        guard !closed, let session else { throw GoLiveEncoderError.unavailable }
        let status = VTSessionSetProperty(session, key: key, value: value)
        guard status == noErr else { throw GoLiveEncoderError.status(status) }
    }
}

private enum GoLiveEncoderError: Error {
    case unavailable
    case status(OSStatus)
}

private func reportError(_ encoder: GoLiveEncoder, _ status: OSStatus) {
    encoder.errorCallback(encoder.callbackContext, Int32(status))
}

private func copyBlockBuffer(_ blockBuffer: CMBlockBuffer) -> Data? {
    let length = CMBlockBufferGetDataLength(blockBuffer)
    guard length > 0 else { return nil }
    var data = Data(count: length)
    let status = data.withUnsafeMutableBytes { bytes in
        guard let baseAddress = bytes.baseAddress else { return OSStatus(-1) }
        return CMBlockBufferCopyDataBytes(
            blockBuffer,
            atOffset: 0,
            dataLength: length,
            destination: baseAddress
        )
    }
    return status == noErr ? data : nil
}

private func h264HeaderLength(_ format: CMFormatDescription) -> Int {
    var headerLength: Int32 = 4
    var parameterSetCount = 0
    let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format,
        parameterSetIndex: 0,
        parameterSetPointerOut: nil,
        parameterSetSizeOut: nil,
        parameterSetCountOut: &parameterSetCount,
        nalUnitHeaderLengthOut: &headerLength
    )
    guard status == noErr, (1...4).contains(Int(headerLength)) else { return 4 }
    return Int(headerLength)
}

private func parameterSets(_ format: CMFormatDescription) -> Data {
    var count = 0
    var headerLength: Int32 = 4
    guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format,
        parameterSetIndex: 0,
        parameterSetPointerOut: nil,
        parameterSetSizeOut: nil,
        parameterSetCountOut: &count,
        nalUnitHeaderLengthOut: &headerLength
    ) == noErr else { return Data() }

    var result = Data()
    for index in 0..<count {
        var parameterSet: UnsafePointer<UInt8>?
        var size = 0
        var setCount = 0
        guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format,
            parameterSetIndex: index,
            parameterSetPointerOut: &parameterSet,
            parameterSetSizeOut: &size,
            parameterSetCountOut: &setCount,
            nalUnitHeaderLengthOut: &headerLength
        ) == noErr, let parameterSet else { continue }
        result.append(contentsOf: withUnsafeBytes(of: UInt32(size).bigEndian) {
            $0.bindMemory(to: UInt8.self)
        })
        result.append(parameterSet, count: size)
    }
    return result
}

private func normalizeAvcc(_ data: Data, headerLength: Int) -> Data? {
    guard (1...4).contains(headerLength) else { return nil }
    var offset = 0
    var result = Data()
    while offset < data.count {
        guard data.count - offset >= headerLength else { return nil }
        var length = 0
        for index in 0..<headerLength {
            length = (length << 8) | Int(data[offset + index])
        }
        offset += headerLength
        guard length > 0, length <= data.count - offset else { return nil }
        result.append(contentsOf: withUnsafeBytes(of: UInt32(length).bigEndian) {
            $0.bindMemory(to: UInt8.self)
        })
        result.append(data[offset..<(offset + length)])
        offset += length
    }
    return result.isEmpty ? nil : result
}

private func containsIDR(in data: Data) -> Bool {
    var offset = 0
    while data.count - offset >= 4 {
        let length = Int(UInt32(data[offset]) << 24)
            | Int(UInt32(data[offset + 1]) << 16)
            | Int(UInt32(data[offset + 2]) << 8)
            | Int(UInt32(data[offset + 3]))
        offset += 4
        guard length > 0, data.count - offset >= length else { return false }
        if data[offset] & 0x1f == 5 { return true }
        offset += length
    }
    return false
}

private func goLiveCompressionOutput(
    outputCallbackRefCon: UnsafeMutableRawPointer?,
    sourceFrameRefCon: UnsafeMutableRawPointer?,
    status: OSStatus,
    infoFlags: VTEncodeInfoFlags,
    sampleBuffer: CMSampleBuffer?
) {
    guard let refcon = outputCallbackRefCon else { return }
    let encoder = Unmanaged<GoLiveEncoder>.fromOpaque(refcon).takeUnretainedValue()
    guard status == noErr else {
        reportError(encoder, status)
        return
    }
    guard let sampleBuffer,
          CMSampleBufferDataIsReady(sampleBuffer),
          let blockBuffer = CMSampleBufferGetDataBuffer(sampleBuffer),
          let blockData = copyBlockBuffer(blockBuffer) else {
        reportError(encoder, -1)
        return
    }

    // H.264 Constrained Baseline only: normalize the AVCC sample to
    // Annex-B and prepend the cached SPS/PPS so every IDR is decodable
    // from a late join or queue-overflow recovery.
    var payload = Data()
    if let formatDescription = CMSampleBufferGetFormatDescription(sampleBuffer),
       let normalized = normalizeAvcc(
           blockData,
           headerLength: h264HeaderLength(formatDescription)
       ) {
        payload.append(parameterSets(formatDescription))
        payload.append(normalized)
    } else {
        reportError(encoder, -1)
        return
    }

    let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
    let isKeyframe = containsIDR(in: payload)
    let keyframe: UInt8 = isKeyframe ? 1 : 0
    encoder.callback(
        encoder.callbackContext,
        payload.withUnsafeBytes { $0.bindMemory(to: UInt8.self).baseAddress },
        payload.count,
        pts.value,
        pts.timescale,
        keyframe
    )
}

@_cdecl("golive_vt_encoder_create")
public func golive_vt_encoder_create(
    _ width: Int32,
    _ height: Int32,
    _ bitrate: Int32,
    _ frameRate: Int32,
    _ codec: Int32,
    _ callback: GoLiveEncodedCallback,
    _ errorCallback: GoLiveEncoderErrorCallback,
    _ callbackContext: UnsafeMutableRawPointer?
) -> UnsafeMutableRawPointer? {
    do {
        let encoder = try GoLiveEncoder(
            width: width,
            height: height,
            bitrate: bitrate,
            frameRate: frameRate,
            codec,
            callback: callback,
            errorCallback: errorCallback,
            callbackContext: callbackContext
        )
        return Unmanaged.passRetained(encoder).toOpaque()
    } catch {
        return nil
    }
}

@_cdecl("golive_vt_encoder_encode")
public func golive_vt_encoder_encode(
    _ handle: UnsafeMutableRawPointer,
    _ pixelBuffer: CVPixelBuffer,
    _ ptsValue: Int64,
    _ ptsTimescale: Int32
) -> Int32 {
    let encoder = Unmanaged<GoLiveEncoder>.fromOpaque(handle).takeUnretainedValue()
    do {
        try encoder.encode(
            pixelBuffer: pixelBuffer,
            pts: CMTime(value: ptsValue, timescale: ptsTimescale)
        )
        return 0
    } catch GoLiveEncoderError.status(let status) {
        return Int32(status)
    } catch {
        return -1
    }
}

@_cdecl("golive_vt_encoder_flush")
public func golive_vt_encoder_flush(_ handle: UnsafeMutableRawPointer) -> Int32 {
    let encoder = Unmanaged<GoLiveEncoder>.fromOpaque(handle).takeUnretainedValue()
    do {
        try encoder.flush()
        return 0
    } catch GoLiveEncoderError.status(let status) {
        return Int32(status)
    } catch {
        return -1
    }
}

@_cdecl("golive_vt_encoder_force_keyframe")
public func golive_vt_encoder_force_keyframe(_ handle: UnsafeMutableRawPointer) -> Int32 {
    let encoder = Unmanaged<GoLiveEncoder>.fromOpaque(handle).takeUnretainedValue()
    do {
        try encoder.forceKeyframe()
        return 0
    } catch GoLiveEncoderError.status(let status) {
        return Int32(status)
    } catch {
        return -1
    }
}

@_cdecl("golive_vt_encoder_set_bitrate")
public func golive_vt_encoder_set_bitrate(
    _ handle: UnsafeMutableRawPointer,
    _ bitrate: Int32
) -> Int32 {
    let encoder = Unmanaged<GoLiveEncoder>.fromOpaque(handle).takeUnretainedValue()
    do {
        try encoder.setBitrate(bitrate)
        return 0
    } catch GoLiveEncoderError.status(let status) {
        return Int32(status)
    } catch {
        return -1
    }
}

@_cdecl("golive_vt_supports_av1")
public func golive_vt_supports_av1() -> Bool {
    if #available(macOS 13.0, *) {
        var probe: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            width: 1280,
            height: 720,
            codecType: kCMVideoCodecType_AV1,
            encoderSpecification: nil,
            imageBufferAttributes: nil,
            compressedDataAllocator: nil,
            outputCallback: { _, _, _, _, _ in },
            refcon: nil,
            compressionSessionOut: &probe
        )
        probe = nil
        return status == noErr
    }
    return false
}

@_cdecl("golive_vt_encoder_destroy")
public func golive_vt_encoder_destroy(_ handle: UnsafeMutableRawPointer) {
    let encoder = Unmanaged<GoLiveEncoder>.fromOpaque(handle).takeUnretainedValue()
    encoder.close()
    Unmanaged<GoLiveEncoder>.fromOpaque(handle).release()
}
