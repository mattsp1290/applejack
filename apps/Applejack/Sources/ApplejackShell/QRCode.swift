import CoreGraphics
import CoreImage
import CoreImage.CIFilterBuiltins
import Foundation

/// Renders the Steam login challenge as a QR code. Pure CoreImage, no view.
public enum QRCode {
    /// A QR code of `payload`, `scale` pixels per module; `nil` only when
    /// CoreImage refuses the payload (empty, or over the QR capacity).
    public static func cgImage(for payload: String, scale: CGFloat = 8) -> CGImage? {
        guard !payload.isEmpty else { return nil }
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(payload.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage else { return nil }
        let scaled = output.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
        return CIContext(options: [.useSoftwareRenderer: true]).createCGImage(scaled, from: scaled.extent)
    }
}
