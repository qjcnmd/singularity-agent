import { createOrbCanvas } from './orbCanvas'

/*
Adapted from https://github.com/amunozdev/voiceorbs
Galaxy Orb: src/registry/orbe/galaxy-orb/galaxy-orb.tsx

MIT License

Copyright (c) 2026 Alexis Munoz

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

export const TWO_PI = Math.PI * 2;
const LAYER_COUNTS = [60, 60, 30];
const ARM_K = 2.35;
export const ARM_R0 = 0.16;

export interface Star {
  a: number;
  r: number;
  size: number;
  layer: number;
  twinkle: number;
  phase: number;
  bright: number;
  tint: number;
  glow: boolean;
}

export const rand = (seed: number) => {
  const x = Math.sin(seed * 12.9898) * 43758.5453;
  return x - Math.floor(x);
};

export const gauss = (seed: number) => {
  const u = Math.max(rand(seed), 1e-6);
  const v = rand(seed + 0.618034);
  return Math.sqrt(-2 * Math.log(u)) * Math.cos(TWO_PI * v);
};

export const armAngle = (r: number, arm: number) =>
  arm * Math.PI + ARM_K * Math.log(Math.max(r, 0.02) / ARM_R0);

const buildStars = (): Star[] => {
  const stars: Star[] = [];
  let n = 0;
  for (let layer = 0; layer < LAYER_COUNTS.length; layer += 1) {
    for (let i = 0; i < LAYER_COUNTS[layer]; i += 1) {
      n += 1;
      const s = n * 7.13;
      const onArm = rand(s + 0.11) < 0.55;
      const r = onArm ? 0.2 + rand(s + 0.23) * 0.72 : Math.sqrt(rand(s + 0.29)) * 0.92;
      const arm = rand(s + 0.31) > 0.5 ? 1 : 0;
      const a = onArm ? armAngle(r, arm) + gauss(s + 0.41) * 0.2 : rand(s + 0.47) * TWO_PI;
      const roll = rand(s + 0.53);
      const bigCut = layer === 2 ? 0.86 : 0.95;
      const midCut = layer === 2 ? 0.55 : 0.7;
      const px =
        roll > bigCut
          ? 1.6 + rand(s + 0.59) * 0.6
          : roll > midCut
            ? 0.8 + rand(s + 0.61) * 0.6
            : 0.4 + rand(s + 0.67) * 0.3;
      const tintRoll = rand(s + 0.71);
      stars.push({
        a,
        r,
        size: px,
        layer,
        twinkle: rand(s + 0.73) < 0.3 ? 1.1 + rand(s + 0.79) * 2.3 : 0,
        phase: rand(s + 0.83) * TWO_PI,
        bright: 0.45 + rand(s + 0.89) * 0.55,
        tint: tintRoll < 0.6 ? 0 : tintRoll < 0.85 ? 1 : 2,
        glow: roll > bigCut,
      });
    }
  }
  return stars;
};

const STARS = buildStars();
export const STAR_LAYERS: [Star[], Star[], Star[]] = [
  STARS.filter((s) => s.layer === 0),
  STARS.filter((s) => s.layer === 1),
  STARS.filter((s) => s.layer === 2),
];

