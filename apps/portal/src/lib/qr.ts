import "server-only";
import QRCode from "qrcode";

/** QR code as inline SVG (navy on white), rendered on the server. */
export async function qrSvg(text: string): Promise<string> {
  return QRCode.toString(text, {
    type: "svg",
    margin: 1,
    errorCorrectionLevel: "M",
    color: { dark: "#243447", light: "#ffffff" },
  });
}
