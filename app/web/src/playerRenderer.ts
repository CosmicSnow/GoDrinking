export type PlayerFrame = {
  seq: number;
  width: number;
  height: number;
  format: 0 | 1 | 2;
  pixels: Uint8Array;
};

const HEADER = 20;
const MAX_DIM = 8192;

export function parsePlayerFrame(buffer: ArrayBuffer): PlayerFrame {
  if (buffer.byteLength < HEADER) throw new Error("Frame GLP2 incompleto");
  const bytes = new Uint8Array(buffer);
  if (bytes[0] !== 0x47 || bytes[1] !== 0x4c || bytes[2] !== 0x50 || bytes[3] !== 0x32) {
    throw new Error("Frame GLP2 inválido");
  }
  const view = new DataView(buffer);
  const seq = view.getUint32(4, true);
  const width = view.getUint32(8, true);
  const height = view.getUint32(12, true);
  const format = view.getUint32(16, true);
  if (!width || !height || width > MAX_DIM || height > MAX_DIM || ![0, 1, 2].includes(format)) {
    throw new Error("Frame GLP2 inválido");
  }
  const y = width * height;
  const payload = format === 0 ? y * 4 : width % 2 || height % 2 ? -1 : y * 3 / 2;
  if (payload < 0 || !Number.isSafeInteger(payload) || buffer.byteLength !== HEADER + payload) {
    throw new Error("Frame GLP2 inválido");
  }
  return { seq, width, height, format: format as 0 | 1 | 2, pixels: new Uint8Array(buffer, HEADER) };
}

function clampByte(value: number): number {
  return value < 0 ? 0 : value > 255 ? 255 : Math.round(value);
}

/** CPU reference path, also used by the 2D fallback and renderer tests. */
export function frameToRgba(frame: PlayerFrame, out?: Uint8ClampedArray<ArrayBufferLike>): Uint8ClampedArray<ArrayBufferLike> {
  const size = frame.width * frame.height * 4;
  const rgba = out && out.length === size ? out : new Uint8ClampedArray(size);
  if (frame.format === 0) {
    rgba.set(frame.pixels);
    return rgba;
  }
  const ySize = frame.width * frame.height;
  const cw = Math.ceil(frame.width / 2);
  const ch = Math.ceil(frame.height / 2);
  const chromaSize = cw * ch;
  const limited = (y: number, u: number, v: number, index: number) => {
    const yy = (y - 16) * 1.164383;
    rgba[index] = clampByte(yy + 1.596027 * (v - 128));
    rgba[index + 1] = clampByte(yy - 0.391762 * (u - 128) - 0.812968 * (v - 128));
    rgba[index + 2] = clampByte(yy + 2.017232 * (u - 128));
    rgba[index + 3] = 255;
  };
  for (let row = 0; row < frame.height; row++) {
    for (let col = 0; col < frame.width; col++) {
      const i = row * frame.width + col;
      const c = (row >> 1) * cw + (col >> 1);
      const u = frame.format === 1 ? frame.pixels[ySize + c] : frame.pixels[ySize + (c << 1)];
      const v = frame.format === 1 ? frame.pixels[ySize + chromaSize + c] : frame.pixels[ySize + (c << 1) + 1];
      limited(frame.pixels[i], u, v, i * 4);
    }
  }
  return rgba;
}

type Renderer = { draw(frame: PlayerFrame): void; dispose(): void; backend: "webgl" | "canvas2d" };

const vertexSource = `attribute vec2 a_position; attribute vec2 a_texcoord; varying vec2 v_texcoord;
void main(){ gl_Position=vec4(a_position,0.0,1.0); v_texcoord=a_texcoord; }`;
const fragmentPrecision = `#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
`;
const yuvSource = fragmentPrecision + `
varying vec2 v_texcoord;
uniform sampler2D u_y; uniform sampler2D u_u; uniform sampler2D u_v;
uniform bool u_nv12;
void main() {
  float y = (texture2D(u_y, v_texcoord).r - 16.0 / 255.0) * (255.0 / 219.0);
  float u = texture2D(u_u, v_texcoord).r - 128.0 / 255.0;
  float v = (u_nv12 ? texture2D(u_u, v_texcoord).a : texture2D(u_v, v_texcoord).r) - 128.0 / 255.0;
  gl_FragColor = vec4(y + 1.596027 * v, y - 0.391762 * u - 0.812968 * v, y + 2.017232 * u, 1.0);
}`;
const rgbaSource = fragmentPrecision + `
varying vec2 v_texcoord; uniform sampler2D u_rgba;
void main() { gl_FragColor = texture2D(u_rgba, v_texcoord); }`;

function shader(gl: WebGLRenderingContext, type: number, source: string): WebGLShader {
  const value = gl.createShader(type);
  if (!value) throw new Error("WebGL shader indisponível");
  gl.shaderSource(value, source); gl.compileShader(value);
  if (!gl.getShaderParameter(value, gl.COMPILE_STATUS)) { gl.deleteShader(value); throw new Error("WebGL shader inválido"); }
  return value;
}

function createWebglRenderer(canvas: HTMLCanvasElement, gl: WebGLRenderingContext): Renderer {
  let buffer: WebGLBuffer | null = null;
  const textures: (WebGLTexture | null)[] = [null, null, null];
  let program: WebGLProgram | null = null;
  let programKind = -1, lastFormat = -1, width = 0, height = 0;
  let lost = false, disposed = false;
  let position = -1, texcoord = -1;
  let yUniform: WebGLUniformLocation | null = null;
  let uUniform: WebGLUniformLocation | null = null;
  let vUniform: WebGLUniformLocation | null = null;
  let nv12Uniform: WebGLUniformLocation | null = null;
  const onLost = (event: Event) => { event.preventDefault(); lost = true; };
  const onRestored = () => {
    // Every GL resource becomes invalid on loss, including the vertex buffer.
    lost = false;
    buffer = null;
    program = null;
    programKind = lastFormat = -1;
    textures.fill(null);
    width = height = 0;
  };
  canvas.addEventListener("webglcontextlost", onLost);
  canvas.addEventListener("webglcontextrestored", onRestored);

  function ensureProgram(format: number) {
    if (!buffer) {
      buffer = gl.createBuffer();
      if (!buffer) throw new Error("WebGL buffer indisponível");
      gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
      // Incoming rows are top to bottom; texture row zero belongs at the top.
      gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([
        -1, 1, 0, 0, 1, 1, 1, 0, -1, -1, 0, 1, 1, -1, 1, 1,
      ]), gl.STATIC_DRAW);
    }
    const kind = format === 0 ? 0 : 1;
    if (program && programKind === kind) return;
    if (program) gl.deleteProgram(program);
    const vs = shader(gl, gl.VERTEX_SHADER, vertexSource);
    const fs = shader(gl, gl.FRAGMENT_SHADER, kind === 0 ? rgbaSource : yuvSource);
    program = gl.createProgram();
    if (!program) throw new Error("WebGL programa indisponível");
    gl.attachShader(program, vs);
    gl.attachShader(program, fs);
    gl.linkProgram(program);
    gl.deleteShader(vs);
    gl.deleteShader(fs);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw new Error("WebGL programa inválido");
    programKind = kind;
    position = gl.getAttribLocation(program, "a_position");
    texcoord = gl.getAttribLocation(program, "a_texcoord");
    gl.useProgram(program);
    if (kind === 0) gl.uniform1i(gl.getUniformLocation(program, "u_rgba"), 0);
    else {
      yUniform = gl.getUniformLocation(program, "u_y");
      uUniform = gl.getUniformLocation(program, "u_u");
      vUniform = gl.getUniformLocation(program, "u_v");
      nv12Uniform = gl.getUniformLocation(program, "u_nv12");
    }
  }

  function upload(unit: number, w: number, h: number, format: number, pixels: Uint8Array, changed: boolean) {
    gl.activeTexture(gl.TEXTURE0 + unit);
    if (!textures[unit]) {
      textures[unit] = gl.createTexture();
      if (!textures[unit]) throw new Error("WebGL textura indisponível");
      gl.bindTexture(gl.TEXTURE_2D, textures[unit]);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      changed = true;
    } else gl.bindTexture(gl.TEXTURE_2D, textures[unit]);
    if (changed) gl.texImage2D(gl.TEXTURE_2D, 0, format, w, h, 0, format, gl.UNSIGNED_BYTE, pixels);
    else gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, w, h, format, gl.UNSIGNED_BYTE, pixels);
  }

  return {
    backend: "webgl",
    draw(frame) {
      if (disposed || lost || gl.isContextLost()) throw new Error("WebGL contexto indisponível");
      if (canvas.width !== frame.width) canvas.width = frame.width;
      if (canvas.height !== frame.height) canvas.height = frame.height;
      ensureProgram(frame.format);
      gl.useProgram(program);
      gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
      const changed = width !== frame.width || height !== frame.height || lastFormat !== frame.format;
      const ySize = frame.width * frame.height;
      if (frame.format === 0) upload(0, frame.width, frame.height, gl.RGBA, frame.pixels, changed);
      else {
        upload(0, frame.width, frame.height, gl.LUMINANCE, frame.pixels.subarray(0, ySize), changed);
        if (frame.format === 1) {
          upload(1, frame.width / 2, frame.height / 2, gl.LUMINANCE, frame.pixels.subarray(ySize, ySize + ySize / 4), changed);
          upload(2, frame.width / 2, frame.height / 2, gl.LUMINANCE, frame.pixels.subarray(ySize + ySize / 4), changed);
        } else {
          // NV12 uploads interleaved UV directly; no JS chroma conversion/copy.
          upload(1, frame.width / 2, frame.height / 2, gl.LUMINANCE_ALPHA, frame.pixels.subarray(ySize), changed);
        }
        gl.uniform1i(yUniform, 0);
        gl.uniform1i(uUniform, 1);
        gl.uniform1i(vUniform, frame.format === 2 ? 1 : 2);
        gl.uniform1i(nv12Uniform, frame.format === 2 ? 1 : 0);
      }
      width = frame.width; height = frame.height; lastFormat = frame.format;
      gl.viewport(0, 0, width, height);
      gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
      gl.enableVertexAttribArray(position);
      gl.enableVertexAttribArray(texcoord);
      gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 16, 0);
      gl.vertexAttribPointer(texcoord, 2, gl.FLOAT, false, 16, 8);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    },
    dispose() {
      disposed = true;
      canvas.removeEventListener("webglcontextlost", onLost);
      canvas.removeEventListener("webglcontextrestored", onRestored);
      textures.forEach(t => { if (t) gl.deleteTexture(t); });
      if (program) gl.deleteProgram(program);
      if (buffer) gl.deleteBuffer(buffer);
    },
  };
}

function createCanvasRenderer(canvas: HTMLCanvasElement): Renderer {
  const context = canvas.getContext("2d");
  if (!context) throw new Error("Canvas 2D indisponível");
  let image: ImageData | undefined;
  let disposed = false;
  return {
    backend: "canvas2d",
    draw(frame) {
      if (disposed) throw new Error("Renderer descartado");
      if (canvas.width !== frame.width) canvas.width = frame.width;
      if (canvas.height !== frame.height) canvas.height = frame.height;
      if (!image || image.width !== frame.width || image.height !== frame.height) image = new ImageData(frame.width, frame.height);
      frameToRgba(frame, image.data);
      context.putImageData(image, 0, 0);
    },
    dispose() { disposed = true; image = undefined; },
  };
}

export function createPlayerRenderer(canvas: HTMLCanvasElement): Renderer {
  const gl = canvas.getContext("webgl", { alpha: false, antialias: false, premultipliedAlpha: false }) as WebGLRenderingContext | null;
  return gl ? createWebglRenderer(canvas, gl) : createCanvasRenderer(canvas);
}
