"use client";

import { useEffect, useRef } from "react";

const GLYPHS = " .·:-=+*ox#%@";
const ATLAS_CELL = 64;

const VERT = `
attribute vec2 aPos;
void main() { gl_Position = vec4(aPos, 0.0, 1.0); }
`;

const FRAG = `
precision highp float;

uniform vec2 uResolution;
uniform vec2 uCell;
uniform vec2 uPointer;
uniform float uTime;
uniform float uCount;
uniform float uMotion;
uniform sampler2D uGlyphs;

float hash(vec2 p) {
  return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453123);
}

float noise(vec2 p) {
  vec2 i = floor(p);
  vec2 f = fract(p);
  vec2 u = f * f * (3.0 - 2.0 * f);
  return mix(
    mix(hash(i), hash(i + vec2(1.0, 0.0)), u.x),
    mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), u.x),
    u.y
  );
}

float fbm(vec2 p) {
  float v = 0.0;
  float a = 0.5;
  for (int i = 0; i < 5; i++) {
    v += a * noise(p);
    p *= 2.02;
    a *= 0.5;
  }
  return v;
}

void main() {
  vec2 cell = floor(gl_FragCoord.xy / uCell);
  vec2 center = (cell + 0.5) * uCell;
  vec2 uv = center / uResolution;
  float aspect = uResolution.x / max(uResolution.y, 1.0);
  vec2 p = vec2(uv.x * aspect, uv.y) * 3.2;

  float t = uTime * 0.06 * uMotion;

  // domain warp: the field folds through itself so the glyph flow never reads as a scroll
  vec2 q = vec2(fbm(p + t), fbm(p + vec2(5.2, 1.3) - t));
  vec2 r = vec2(
    fbm(p + 3.4 * q + vec2(1.7, 9.2) + 0.15 * t),
    fbm(p + 3.4 * q + vec2(8.3, 2.8) - 0.13 * t)
  );
  float field = fbm(p + 3.4 * r);

  // pointer swell, in the same warped space so it bends with the flow
  vec2 pointer = uPointer / uResolution;
  float d = distance(vec2(uv.x * aspect, uv.y), vec2(pointer.x * aspect, pointer.y));
  field += 0.28 * exp(-d * 7.0);

  // clear the middle so the headline never fights the field
  vec2 fromCenter = (uv - 0.5) * vec2(aspect, 1.0);
  float clearing = smoothstep(0.16, 0.62, length(fromCenter));
  float edges = smoothstep(0.0, 0.14, uv.x) * smoothstep(0.0, 0.14, 1.0 - uv.x)
              * smoothstep(0.0, 0.18, uv.y) * smoothstep(0.0, 0.18, 1.0 - uv.y);

  float intensity = clamp(field * 1.35 - 0.18, 0.0, 1.0) * clearing * mix(0.35, 1.0, edges);

  float index = floor(intensity * (uCount - 0.001));
  vec2 inCell = fract(gl_FragCoord.xy / uCell);
  vec2 atlas = vec2((index + inCell.x) / uCount, 1.0 - inCell.y);
  float glyph = texture2D(uGlyphs, atlas).r;

  vec3 warm = vec3(0.62, 0.66, 0.78);
  vec3 cool = vec3(0.40, 0.46, 0.62);
  vec3 tint = mix(cool, warm, intensity);

  gl_FragColor = vec4(tint * glyph * (0.16 + intensity * 0.42), 1.0);
}
`;

/** Canvas 2D cannot parse `var(--font)`, so resolve the family to a literal first. */
function glyphFontFamily(): string {
  const resolved = getComputedStyle(document.body)
    .fontFamily.split(",")
    .map((part) => part.trim())
    .filter((part) => !part.startsWith("var("))
    .join(", ");
  return resolved || "ui-monospace, monospace";
}

function buildAtlas(): HTMLCanvasElement {
  const canvas = document.createElement("canvas");
  canvas.width = ATLAS_CELL * GLYPHS.length;
  canvas.height = ATLAS_CELL;
  const ctx = canvas.getContext("2d");
  if (!ctx) return canvas;
  ctx.fillStyle = "#000";
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.fillStyle = "#fff";
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.font = `${Math.round(ATLAS_CELL * 0.74)}px ${glyphFontFamily()}`;
  for (let i = 0; i < GLYPHS.length; i++) {
    ctx.fillText(GLYPHS[i]!, i * ATLAS_CELL + ATLAS_CELL / 2, ATLAS_CELL / 2 + 1);
  }
  return canvas;
}

function compile(gl: WebGLRenderingContext, type: number, source: string): WebGLShader | null {
  const shader = gl.createShader(type);
  if (!shader) return null;
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    gl.deleteShader(shader);
    return null;
  }
  return shader;
}

export function AsciiField() {
  const ref = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;

    const gl = (canvas.getContext("webgl2", { antialias: false, alpha: false }) ??
      canvas.getContext("webgl", { antialias: false, alpha: false })) as WebGLRenderingContext | null;
    if (!gl) return;

    const vert = compile(gl, gl.VERTEX_SHADER, VERT);
    const frag = compile(gl, gl.FRAGMENT_SHADER, FRAG);
    if (!vert || !frag) return;

    const program = gl.createProgram();
    if (!program) return;
    gl.attachShader(program, vert);
    gl.attachShader(program, frag);
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) return;
    gl.useProgram(program);

    const buffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const aPos = gl.getAttribLocation(program, "aPos");
    gl.enableVertexAttribArray(aPos);
    gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, 0, 0);

    const texture = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);

    const uploadAtlas = () => {
      gl.bindTexture(gl.TEXTURE_2D, texture);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, buildAtlas());
    };
    uploadAtlas();
    // the pixel font may still be loading; redraw the atlas once it lands
    void document.fonts?.ready.then(uploadAtlas).catch(() => {});

    const uniform = (name: string) => gl.getUniformLocation(program, name);
    const uResolution = uniform("uResolution");
    const uCell = uniform("uCell");
    const uPointer = uniform("uPointer");
    const uTime = uniform("uTime");
    const uCount = uniform("uCount");
    const uMotion = uniform("uMotion");
    gl.uniform1i(uniform("uGlyphs"), 0);
    gl.uniform1f(uCount, GLYPHS.length);

    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)");
    let motion = reduced.matches ? 0 : 1;
    const onMotionChange = () => {
      motion = reduced.matches ? 0 : 1;
    };
    reduced.addEventListener("change", onMotionChange);

    const pointer = { x: 0, y: 0, tx: 0, ty: 0 };
    const onPointer = (event: PointerEvent) => {
      pointer.tx = event.clientX * (window.devicePixelRatio || 1);
      pointer.ty = (window.innerHeight - event.clientY) * (window.devicePixelRatio || 1);
    };
    window.addEventListener("pointermove", onPointer, { passive: true });

    const resize = () => {
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      const width = Math.floor(window.innerWidth * dpr);
      const height = Math.floor(window.innerHeight * dpr);
      if (canvas.width === width && canvas.height === height) return;
      canvas.width = width;
      canvas.height = height;
      gl.viewport(0, 0, width, height);
      gl.uniform2f(uResolution, width, height);
      const size = Math.max(9, Math.round(11 * dpr));
      gl.uniform2f(uCell, size, Math.round(size * 1.15));
      pointer.x = width / 2;
      pointer.y = height * 0.62;
      pointer.tx = pointer.x;
      pointer.ty = pointer.y;
    };
    resize();
    window.addEventListener("resize", resize);

    let raf = 0;
    let running = true;
    const start = performance.now();
    const frame = (now: number) => {
      if (!running) return;
      pointer.x += (pointer.tx - pointer.x) * 0.045;
      pointer.y += (pointer.ty - pointer.y) * 0.045;
      gl.uniform2f(uPointer, pointer.x, pointer.y);
      gl.uniform1f(uTime, (now - start) / 1000);
      gl.uniform1f(uMotion, motion);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      raf = requestAnimationFrame(frame);
    };
    raf = requestAnimationFrame(frame);

    const onVisibility = () => {
      if (document.hidden) {
        running = false;
        cancelAnimationFrame(raf);
      } else if (!running) {
        running = true;
        raf = requestAnimationFrame(frame);
      }
    };
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      running = false;
      cancelAnimationFrame(raf);
      window.removeEventListener("resize", resize);
      window.removeEventListener("pointermove", onPointer);
      document.removeEventListener("visibilitychange", onVisibility);
      reduced.removeEventListener("change", onMotionChange);
      gl.deleteTexture(texture);
      gl.deleteBuffer(buffer);
      gl.deleteProgram(program);
      gl.deleteShader(vert);
      gl.deleteShader(frag);
    };
  }, []);

  return <canvas ref={ref} className="ascii-field" aria-hidden="true" />;
}
