// Hero: a tilted 3D Mac screen you can draw on. The screen is a canvas texture;
// strokes (yours or the scripted demo) are drawn onto it and fade like TapeIt's fading ink.
//
//   pointer ──raycast──▶ screen UV ──▶ canvas stroke ──▶ CanvasTexture ──▶ WebGL
import * as THREE from 'three';

const TEX_W = 1600;
const TEX_H = 1000;
const SCREEN_W = 4.0;
const SCREEN_H = SCREEN_W * (TEX_H / TEX_W);
const BEZEL = 0.12;
const FADE_AFTER_MS = 3500;
const FADE_MS = 900;
const IDLE_BEFORE_DEMO_MS = 2500;
const DEMO_GAP_MS = 1400;
const YELLOW = '#ffd60a';
const RED = '#ff3b30';

const stage = document.getElementById('stage');
const hint = document.getElementById('hint');
const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)').matches;

// ---- screen content: a fake slide in a Mac window ----
const canvas = document.createElement('canvas');
canvas.width = TEX_W;
canvas.height = TEX_H;
const context = canvas.getContext('2d');

function drawBase() {
  const gradient = context.createLinearGradient(0, 0, TEX_W, TEX_H);
  gradient.addColorStop(0, '#1d2170');
  gradient.addColorStop(1, '#0c0f33');
  context.fillStyle = gradient;
  context.fillRect(0, 0, TEX_W, TEX_H);

  // Window
  context.fillStyle = '#ffffff';
  roundRect(120, 110, 1360, 800, 26);
  context.fill();
  context.fillStyle = '#eef0f8';
  roundRect(120, 110, 1360, 64, 26);
  context.fill();
  context.fillRect(120, 150, 1360, 24);
  ['#ff5f57', '#febc2e', '#28c840'].forEach((color, index) => {
    context.fillStyle = color;
    context.beginPath();
    context.arc(162 + index * 34, 142, 10, 0, Math.PI * 2);
    context.fill();
  });

  // Slide
  context.fillStyle = '#14163a';
  context.font = '700 76px "Bricolage Grotesque", sans-serif';
  context.fillText('Launch plan', 210, 300);
  context.fillStyle = '#5d6084';
  context.font = '500 34px "Instrument Sans", sans-serif';
  context.fillText('Signups per week', 212, 360);

  const bars = [180, 240, 210, 320, 420];
  bars.forEach((height, index) => {
    context.fillStyle = index === bars.length - 1 ? '#3a3fb0' : '#c9ccec';
    roundRect(220 + index * 130, 800 - height, 86, height, 12);
    context.fill();
  });

  context.fillStyle = '#ff3b30';
  roundRect(1000, 690, 330, 96, 48);
  context.fill();
  context.fillStyle = '#ffffff';
  context.font = '600 38px "Instrument Sans", sans-serif';
  context.fillText('Ship it', 1105, 752);
}

function roundRect(x, y, width, height, radius) {
  context.beginPath();
  context.roundRect(x, y, width, height, radius);
}

// ---- strokes ----
const strokes = [];
let current = null;
let lastUserInk = 0;

function strokeAlpha(stroke, now) {
  const age = now - stroke.done;
  if (!stroke.done || age < FADE_AFTER_MS) {
    return 1;
  }
  return Math.max(0, 1 - (age - FADE_AFTER_MS) / FADE_MS);
}

function drawStrokes(now) {
  for (const stroke of strokes) {
    const alpha = strokeAlpha(stroke, now);
    if (alpha <= 0 || stroke.points.length < 2) {
      continue;
    }
    context.globalAlpha = alpha * stroke.opacity;
    context.strokeStyle = stroke.color;
    context.lineWidth = stroke.width;
    context.lineCap = 'round';
    context.lineJoin = 'round';
    context.beginPath();
    context.moveTo(stroke.points[0].x, stroke.points[0].y);
    for (const point of stroke.points.slice(1)) {
      context.lineTo(point.x, point.y);
    }
    context.stroke();
    if (stroke.arrow) {
      drawArrowHead(stroke);
    }
  }
  context.globalAlpha = 1;

  // Drop fully faded strokes.
  for (let index = strokes.length - 1; index >= 0; index -= 1) {
    if (strokes[index].done && strokeAlpha(strokes[index], now) <= 0) {
      strokes.splice(index, 1);
    }
  }
}

function drawArrowHead(stroke) {
  const points = stroke.points;
  const tip = points[points.length - 1];
  const back = points[Math.max(0, points.length - 6)];
  const angle = Math.atan2(tip.y - back.y, tip.x - back.x);
  const size = 34;
  context.beginPath();
  context.moveTo(tip.x - size * Math.cos(angle - 0.5), tip.y - size * Math.sin(angle - 0.5));
  context.lineTo(tip.x, tip.y);
  context.lineTo(tip.x - size * Math.cos(angle + 0.5), tip.y - size * Math.sin(angle + 0.5));
  context.stroke();
}

// ---- scripted demo: highlight the title, circle the tall bar, arrow to the button ----
const DEMO = [
  { color: YELLOW, width: 34, opacity: 0.55, path: (t) => ({ x: 205 + t * 470, y: 318 + Math.sin(t * 6) * 5 }) },
  { color: RED, width: 10, opacity: 1, path: (t) => ({ x: 783 + Math.cos(t * Math.PI * 2.1 - 1.6) * 95, y: 590 + Math.sin(t * Math.PI * 2.1 - 1.6) * 250 }) },
  { color: RED, width: 10, opacity: 1, arrow: true, path: (t) => ({ x: 930 + t * 120, y: 470 + t * 200 - Math.sin(t * Math.PI) * 60 }) },
];
let demoIndex = 0;
let demoStroke = null;
let demoStart = 0;
let nextDemoAt = 800;
const DEMO_DRAW_MS = 900;

function runDemo(now) {
  if (now - lastUserInk < IDLE_BEFORE_DEMO_MS) {
    return;
  }
  if (!demoStroke && now >= nextDemoAt) {
    const spec = DEMO[demoIndex % DEMO.length];
    demoStroke = { ...spec, points: [], done: 0 };
    strokes.push(demoStroke);
    demoStart = now;
  }
  if (!demoStroke) {
    return;
  }
  const progress = Math.min(1, (now - demoStart) / DEMO_DRAW_MS);
  const eased = 1 - Math.pow(1 - progress, 3);
  demoStroke.points.push(demoStroke.path(eased));
  if (progress >= 1) {
    demoStroke.done = now;
    demoStroke = null;
    demoIndex += 1;
    nextDemoAt = now + (demoIndex % DEMO.length === 0 ? DEMO_GAP_MS + FADE_AFTER_MS : DEMO_GAP_MS);
  }
}

// ---- three.js scene ----
const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
renderer.outputColorSpace = THREE.SRGBColorSpace;
stage.prepend(renderer.domElement);

const scene = new THREE.Scene();
const camera = new THREE.PerspectiveCamera(32, 1, 0.1, 100);
camera.position.set(0, 0, 9);

const device = new THREE.Group();
scene.add(device);

// Rounded bezel behind the screen.
const bezelShape = new THREE.Shape();
const halfW = SCREEN_W / 2 + BEZEL;
const halfH = SCREEN_H / 2 + BEZEL;
const corner = 0.22;
bezelShape.moveTo(-halfW + corner, -halfH);
bezelShape.lineTo(halfW - corner, -halfH);
bezelShape.quadraticCurveTo(halfW, -halfH, halfW, -halfH + corner);
bezelShape.lineTo(halfW, halfH - corner);
bezelShape.quadraticCurveTo(halfW, halfH, halfW - corner, halfH);
bezelShape.lineTo(-halfW + corner, halfH);
bezelShape.quadraticCurveTo(-halfW, halfH, -halfW, halfH - corner);
bezelShape.lineTo(-halfW, -halfH + corner);
bezelShape.quadraticCurveTo(-halfW, -halfH, -halfW + corner, -halfH);
const bezel = new THREE.Mesh(
  new THREE.ExtrudeGeometry(bezelShape, { depth: 0.14, bevelEnabled: true, bevelSize: 0.03, bevelThickness: 0.03, bevelSegments: 4 }),
  new THREE.MeshStandardMaterial({ color: 0x1a1c2e, metalness: 0.6, roughness: 0.35 }),
);
bezel.position.z = -0.16;
device.add(bezel);

const texture = new THREE.CanvasTexture(canvas);
texture.colorSpace = THREE.SRGBColorSpace;
texture.anisotropy = renderer.capabilities.getMaxAnisotropy();
const screen = new THREE.Mesh(new THREE.PlaneGeometry(SCREEN_W, SCREEN_H), new THREE.MeshBasicMaterial({ map: texture }));
screen.position.z = 0.02;
device.add(screen);

// Soft red glow from the record light.
scene.add(new THREE.AmbientLight(0xffffff, 0.7));
const key = new THREE.DirectionalLight(0xffffff, 1.4);
key.position.set(3, 4, 6);
scene.add(key);
const rim = new THREE.PointLight(0xff3b30, 18, 12);
rim.position.set(-3.2, 2.2, 1.5);
scene.add(rim);

const BASE_TILT = { x: 0.1, y: -0.32 };
device.rotation.set(BASE_TILT.x, BASE_TILT.y, 0);

function resize() {
  const { width, height } = stage.getBoundingClientRect();
  renderer.setSize(width, height, false);
  camera.aspect = width / height;
  // Narrow stages pull the camera back so the whole screen fits.
  camera.position.z = width / height < 1.1 ? 9 : 6.6;
  camera.updateProjectionMatrix();
}
new ResizeObserver(resize).observe(stage);
resize();

// ---- pointer drawing via raycast onto the screen ----
const raycaster = new THREE.Raycaster();
const pointer = new THREE.Vector2();
let parallax = { x: 0, y: 0 };

function screenPoint(event) {
  const rect = renderer.domElement.getBoundingClientRect();
  pointer.set(((event.clientX - rect.left) / rect.width) * 2 - 1, -((event.clientY - rect.top) / rect.height) * 2 + 1);
  raycaster.setFromCamera(pointer, camera);
  const hit = raycaster.intersectObject(screen)[0];
  if (!hit) {
    return null;
  }
  return { x: hit.uv.x * TEX_W, y: (1 - hit.uv.y) * TEX_H };
}

stage.addEventListener('pointerdown', (event) => {
  const point = screenPoint(event);
  if (!point) {
    return;
  }
  stage.setPointerCapture(event.pointerId);
  current = { color: YELLOW, width: 16, opacity: 1, points: [point], done: 0 };
  strokes.push(current);
  lastUserInk = performance.now();
  hint.style.opacity = '0';
});

stage.addEventListener('pointermove', (event) => {
  const rect = stage.getBoundingClientRect();
  parallax = { x: (event.clientX - rect.left) / rect.width - 0.5, y: (event.clientY - rect.top) / rect.height - 0.5 };
  if (!current) {
    return;
  }
  const point = screenPoint(event);
  if (point) {
    current.points.push(point);
    lastUserInk = performance.now();
  }
});

function endStroke() {
  if (!current) {
    return;
  }
  current.done = performance.now();
  current = null;
}
stage.addEventListener('pointerup', endStroke);
stage.addEventListener('pointercancel', endStroke);
stage.addEventListener('pointerleave', () => {
  parallax = { x: 0, y: 0 };
});

// ---- loop: only render while the hero is on screen ----
let visible = true;
new IntersectionObserver(([entry]) => {
  visible = entry.isIntersecting;
}).observe(stage);

function frame(now) {
  requestAnimationFrame(frame);
  if (!visible) {
    return;
  }

  runDemo(now);
  drawBase();
  drawStrokes(now);
  texture.needsUpdate = true;

  // While drawing, hold the screen still so strokes land where the pointer is.
  if (!reducedMotion && !current) {
    const float = Math.sin(now / 1600) * 0.04;
    device.rotation.x += (BASE_TILT.x + parallax.y * 0.12 - device.rotation.x) * 0.06;
    device.rotation.y += (BASE_TILT.y + parallax.x * 0.2 - device.rotation.y) * 0.06;
    device.position.y = float;
  }
  renderer.render(scene, camera);
}

document.fonts.ready.then(() => requestAnimationFrame(frame));
