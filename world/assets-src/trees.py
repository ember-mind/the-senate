# Builds world/assets/trees.glb: a Mediterranean cypress and an umbrella
# pine, each as a foliage part and a trunk part, instanced by
# world/js/architecture.js. Run headless:
#   blender -b --factory-startup --python world/assets-src/trees.py
# Authored in three.js space (y up, see figures.py); origin at the trunk base.

import bmesh
import bpy
import math
import os
import random
from mathutils import Vector, noise

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'assets', 'trees.glb')
bpy.ops.wm.read_factory_settings(use_empty=True)
random.seed(7)


def T(x, y, z):
    return Vector((x, -z, y))


def obj(name, bm, subsurf=0, smooth=True):
    me = bpy.data.meshes.new(name)
    bm.to_mesh(me)
    bm.free()
    for p in me.polygons:
        p.use_smooth = smooth
    ob = bpy.data.objects.new(name, me)
    bpy.context.collection.objects.link(ob)
    if subsurf:
        m = ob.modifiers.new('sub', 'SUBSURF')
        m.levels = subsurf
    return ob


def blob(bm, c, r, squash=(1, 1, 1), lump=0.28, seed=0.0, subdiv=2):
    """A lumpy foliage clump: an icosphere pushed by noise."""
    ret = bmesh.ops.create_icosphere(bm, subdivisions=subdiv, radius=1.0)
    for v in ret['verts']:
        d = Vector((v.co.x, v.co.z, -v.co.y))  # three space unit direction
        n = noise.noise(d * 2.3 + Vector((seed, seed * 0.7, seed * 1.3)))
        k = 1 + lump * n
        p = Vector((c[0] + d.x * r * squash[0] * k, c[1] + d.y * r * squash[1] * k, c[2] + d.z * r * squash[2] * k))
        v.co = T(p.x, p.y, p.z)
    # Each clump is closed on its own: orient its faces outward by itself,
    # before overlapping clumps can confuse a whole-mesh recalculation.
    faces = list({f for v in ret['verts'] for f in v.link_faces})
    bmesh.ops.recalc_face_normals(bm, faces=faces)


def trunk(name, height, r0, r1, bend=(0, 0), segs=8, rings=8):
    bm = bmesh.new()
    loops = []
    for i in range(rings + 1):
        t = i / rings
        y = height * t
        r = r0 + (r1 - r0) * t
        ox = bend[0] * math.sin(t * math.pi * 0.5)
        oz = bend[1] * math.sin(t * math.pi * 0.5)
        ring = []
        for j in range(segs):
            a = 2 * math.pi * j / segs
            rr = r * (1 + 0.08 * noise.noise(Vector((a, y * 3, 0.5))))
            ring.append(bm.verts.new(T(ox + math.sin(a) * rr, y, oz + math.cos(a) * rr)))
        loops.append(ring)
    for a, b in zip(loops, loops[1:]):
        for j in range(segs):
            bm.faces.new((a[j], a[(j + 1) % segs], b[(j + 1) % segs], b[j]))
    bm.faces.new(list(reversed(loops[0])))
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    return obj(name, bm)


# Cypress: a tall flame of overlapping clumps, widest a third of the way up.
bm = bmesh.new()
H = 7.0
for i in range(26):
    t = i / 25
    y = 0.5 + t * (H - 0.6)
    width = 0.95 * math.sin(min(1.0, (t + 0.08) / 1.08) * math.pi) ** 0.8 * (1 - 0.35 * t)
    for k in range(2 if t < 0.85 else 1):
        a = random.random() * math.tau
        off = width * 0.35
        blob(bm, (math.cos(a) * off, y, math.sin(a) * off), 0.28 + width * 0.38, (1, 1.5, 1), 0.3, seed=i * 3.1 + k, subdiv=2)
obj('cypressFoliage', bm)
trunk('cypressTrunk', 1.2, 0.12, 0.07)

# Umbrella pine: a tall bare trunk leaning slightly, a broad flat canopy made
# of a ring of clumps with a thinner top.
bm = bmesh.new()
top = 7.6
lean = (0.9, 0.3)
for i in range(18):
    a = i / 18 * math.tau + random.random() * 0.3
    rr = 1.2 + random.random() * 1.7
    cx, cz = lean[0] + math.cos(a) * rr, lean[1] + math.sin(a) * rr
    cy = top + random.random() * 0.6 - rr * 0.12
    blob(bm, (cx, cy, cz), 0.9 + random.random() * 0.5, (1.25, 0.5, 1.25), 0.3, seed=i * 5.7, subdiv=2)
for i in range(5):
    a = random.random() * math.tau
    blob(bm, (lean[0] + math.cos(a) * 0.8, top + 0.55, lean[1] + math.sin(a) * 0.8), 1.2, (1.3, 0.42, 1.3), 0.25, seed=40 + i, subdiv=2)
obj('pineCanopy', bm)
trunk('pineTrunk', top + 0.1, 0.24, 0.12, bend=lean, segs=9)
# Two branches into the canopy.
bm = bmesh.new()
for sgn in (-1, 1):
    base = Vector((lean[0] * 0.8, top - 1.4, lean[1] * 0.8))
    tip = Vector((lean[0] + sgn * 1.6, top - 0.1, lean[1] - sgn * 0.6))
    d = (tip - base)
    L = d.length
    ret = bmesh.ops.create_cone(bm, cap_ends=True, segments=6, radius1=0.09, radius2=0.04, depth=L)
    # cone is along Blender Z; orient to d (in three space -> Blender)
    q = Vector((0, 0, 1)).rotation_difference(T(d.x, d.y, d.z).normalized())
    for v in ret['verts']:
        v.co = q @ v.co + T(*(base + d * 0.5))
obj('pineBranches', bm)

os.makedirs(os.path.dirname(OUT), exist_ok=True)
bpy.ops.export_scene.gltf(filepath=OUT, export_format='GLB', export_yup=True, export_apply=True, export_materials='NONE', export_texcoords=False)
print('wrote', OUT, os.path.getsize(OUT))
