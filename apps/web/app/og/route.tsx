import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { ImageResponse } from "@takumi-rs/image-response";

export const dynamic = "force-static";
export const revalidate = false;

const GLYPHS = " .·:-=+*ox#%@";
const COLS = 50;
const ROWS = 27;
const CELL = 24;

function hash(x: number, y: number): number {
  const n = Math.sin(x * 127.1 + y * 311.7) * 43758.5453123;
  return n - Math.floor(n);
}

function noise(x: number, y: number): number {
  const ix = Math.floor(x);
  const iy = Math.floor(y);
  const fx = x - ix;
  const fy = y - iy;
  const ux = fx * fx * (3 - 2 * fx);
  const uy = fy * fy * (3 - 2 * fy);
  const a = hash(ix, iy);
  const b = hash(ix + 1, iy);
  const c = hash(ix, iy + 1);
  const d = hash(ix + 1, iy + 1);
  return (a + (b - a) * ux) * (1 - uy) + (c + (d - c) * ux) * uy;
}

function fbm(x: number, y: number): number {
  let value = 0;
  let amplitude = 0.5;
  let px = x;
  let py = y;
  for (let i = 0; i < 5; i++) {
    value += amplitude * noise(px, py);
    px *= 2.02;
    py *= 2.02;
    amplitude *= 0.5;
  }
  return value;
}

/** Same domain-warped field as the live background, sampled once per character cell. */
function fieldAt(col: number, row: number): number {
  const x = (col / COLS) * 6.4;
  const y = (row / ROWS) * 3.4;
  const qx = fbm(x, y);
  const qy = fbm(x + 5.2, y + 1.3);
  const rx = fbm(x + 3.4 * qx + 1.7, y + 3.4 * qy + 9.2);
  const ry = fbm(x + 3.4 * qx + 8.3, y + 3.4 * qy + 2.8);
  const field = fbm(x + 3.4 * rx, y + 3.4 * ry);

  const dx = (col / COLS - 0.5) * 1.9;
  const dy = row / ROWS - 0.5;
  const distance = Math.sqrt(dx * dx + dy * dy);
  const clearing = Math.min(1, Math.max(0, (distance - 0.26) / 0.38));

  return Math.min(1, Math.max(0, (field - 0.34) * 2.6)) * clearing;
}

const FONT_PATH = "geist/dist/fonts/geist-pixel/GeistPixel-Square.woff2";

/** geist's exports map blocks deep paths under Node, so read the file directly. */
function pixelFont(): Uint8Array | null {
  for (const base of ["node_modules", "../../node_modules", "../node_modules"]) {
    const candidate = join(process.cwd(), base, FONT_PATH);
    if (existsSync(candidate)) return new Uint8Array(readFileSync(candidate));
  }
  return null;
}

export function GET() {
  const cells = [];
  for (let row = 0; row < ROWS; row++) {
    for (let col = 0; col < COLS; col++) {
      const intensity = fieldAt(col, row);
      if (intensity < 0.06) continue;
      const index = Math.min(GLYPHS.length - 1, Math.floor(intensity * GLYPHS.length));
      const glyph = GLYPHS[index]!;
      if (glyph === " ") continue;
      cells.push(
        <div
          key={`${col}-${row}`}
          style={{
            position: "absolute",
            left: col * CELL,
            top: row * CELL,
            width: CELL,
            height: CELL,
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            fontSize: 15,
            color: `rgba(190, 200, 225, ${(0.06 + intensity * 0.2).toFixed(3)})`,
          }}
        >
          {glyph}
        </div>,
      );
    }
  }

  const font = pixelFont();

  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          position: "relative",
          backgroundColor: "#000",
          fontFamily: "KyoraPixel",
        }}
      >
        <div style={{ position: "absolute", inset: 0, display: "flex" }}>{cells}</div>
        <div
          style={{
            position: "absolute",
            inset: 0,
            display: "flex",
            flexDirection: "column",
            alignItems: "center",
            justifyContent: "center",
            gap: 24,
          }}
        >
          <div style={{ fontSize: 96, color: "#fff", letterSpacing: -1 }}>Kyora</div>
          <div style={{ fontSize: 26, color: "rgba(255,255,255,0.6)" }}>
            Unlocking the full potential of coding agents.
          </div>
          <div style={{ display: "flex", gap: 12, marginTop: 30 }}>
            {["state", "review", "council"].map((name) => (
              <div
                key={name}
                style={{
                  display: "flex",
                  fontSize: 17,
                  color: "rgba(255,255,255,0.72)",
                  letterSpacing: 3,
                  padding: "9px 20px",
                  border: "1px solid rgba(255,255,255,0.16)",
                }}
              >
                {name}
              </div>
            ))}
          </div>
        </div>
      </div>
    ),
    {
      width: 1200,
      height: 630,
      format: "png",
      ...(font ? { fonts: [{ name: "KyoraPixel", data: font }] } : {}),
    },
  );
}
