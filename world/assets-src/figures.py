# Builds world/assets/figures.glb: the named body and garment parts the
# figure rig in world/js/figures.js assembles. Run headless:
#   blender -b --factory-startup --python world/assets-src/figures.py
# Every object is authored in three.js space rotated into Blender's:
# three +Y (up) = Blender +Z, three +Z (forward, the way a figure faces) =
# Blender -Y. The exporter's +Y-up conversion brings it back. Origins sit
# exactly where the rig's pivots expect them, so parts swap in one-for-one.

import bmesh
import bpy
import math
import os
from mathutils import Vector

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'assets', 'figures.glb')

bpy.ops.wm.read_factory_settings(use_empty=True)


def T(x, y, z):
    """three.js coordinates -> Blender coordinates."""
    return Vector((x, -z, y))


def new_object(name, bm, smooth=True, subsurf=1):
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
        m.render_levels = subsurf
    return ob


def rings(name, profile, segs=24, radial=None, cap_top=True, cap_bottom=False, subsurf=1, arc=None):
    """Lofts rings. profile: list of (y, rx, rz[, dx, dz]) in three space.
    radial(theta, y, r) -> multiplier for fold displacement.
    arc: (a0, a1) partial ring (open), angle 0 = forward (+Z three)."""
    bm = bmesh.new()
    loops = []
    for row in profile:
        y, rx, rz = row[0], row[1], row[2]
        dx = row[3] if len(row) > 3 else 0.0
        dz = row[4] if len(row) > 4 else 0.0
        ring = []
        n = segs if arc is None else segs + 1
        for i in range(n):
            if arc is None:
                a = 2 * math.pi * i / segs
            else:
                a = arc[0] + (arc[1] - arc[0]) * i / segs
            k = radial(a, y, rx) if radial else 1.0
            x = math.sin(a) * rx * k + dx
            z = math.cos(a) * rz * k + dz
            ring.append(bm.verts.new(T(x, y, z)))
        loops.append(ring)
    for r0, r1 in zip(loops, loops[1:]):
        n = len(r0)
        m = n if arc is None else n - 1
        for i in range(m):
            j = (i + 1) % n
            bm.faces.new((r0[i], r0[j], r1[j], r1[i]))
    if cap_top and arc is None:
        bm.faces.new(list(reversed(loops[-1])) if profile[-1][0] > profile[0][0] else loops[-1])
    if cap_bottom and arc is None:
        bm.faces.new(loops[0] if profile[-1][0] > profile[0][0] else list(reversed(loops[0])))
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    return new_object(name, bm, subsurf=subsurf)


def ellipsoid(bm, cx, cy, cz, rx, ry, rz, segs=16, rings_=10):
    ret = bmesh.ops.create_uvsphere(bm, u_segments=segs, v_segments=rings_, radius=1.0)
    for v in ret['verts']:
        # created in Blender space; interpret as three space unit sphere
        tx, ty, tz = v.co.x, v.co.z, -v.co.y
        v.co = T(cx + tx * rx, cy + ty * ry, cz + tz * rz)
    return ret['verts']


# ---------------------------------------------------------------- head
def head():
    bm = bmesh.new()
    ret = bmesh.ops.create_uvsphere(bm, u_segments=28, v_segments=18, radius=1.0)
    for v in ret['verts']:
        x, y, z = v.co.x, v.co.z, -v.co.y  # three space unit sphere
        r = 0.13
        # Skull a touch taller than wide, back of head fuller.
        sx, sy, sz = 0.94, 1.12, 1.0
        # Jaw: narrower and a little forward below the cheekbones.
        if y < 0:
            j = min(1.0, -y * 1.4)
            sx *= 1 - 0.22 * j
            sz *= 1 - 0.08 * j
            z += 0.12 * j * max(0.0, z)
        # Flatten the face plane slightly.
        if z > 0.6:
            z = 0.6 + (z - 0.6) * 0.6
        v.co = T(x * r * sx, y * r * sy, z * r * sz)
    # Nose: a small wedge.
    nose = ellipsoid(bm, 0, -0.012, 0.128, 0.022, 0.034, 0.03, 10, 8)
    for v in nose:
        p = Vector((v.co.x, v.co.z, -v.co.y))
        if p.y < -0.02:
            p.z += 0.012
        v.co = T(p.x, p.y, p.z)
    # Ears.
    for s in (-1, 1):
        ellipsoid(bm, s * 0.118, 0.0, -0.005, 0.016, 0.034, 0.026, 10, 8)
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    return new_object('head', bm, subsurf=1)


def eyes():
    # Dark almond eyes set into the face, and a short mouth line below the
    # nose; both take the same dark material.
    bm = bmesh.new()
    for s in (-1, 1):
        ellipsoid(bm, s * 0.044, 0.016, 0.114, 0.017, 0.013, 0.009, 12, 8)
    vs = ellipsoid(bm, 0, -0.058, 0.117, 0.024, 0.0045, 0.007, 10, 6)
    for v in vs:
        p = Vector((v.co.x, v.co.z, -v.co.y))
        p.y += (p.x * p.x) * 8
        v.co = T(p.x, p.y, p.z)
    return new_object('eyes', bm, subsurf=0)


def brows():
    bm = bmesh.new()
    for s in (-1, 1):
        vs = ellipsoid(bm, s * 0.045, 0.047, 0.112, 0.026, 0.006, 0.009, 10, 6)
        for v in vs:
            p = Vector((v.co.x, v.co.z, -v.co.y))
            p.y += (abs(p.x) - 0.045) * -0.25 * s * s
            v.co = T(p.x, p.y, p.z)
    return new_object('brows', bm, subsurf=0)


def hair(name, cut_front=0.035, cut_low=-0.06, tuft=0.012, bald_top=False, r=0.139):
    bm = bmesh.new()
    ret = bmesh.ops.create_uvsphere(bm, u_segments=28, v_segments=18, radius=1.0)
    kill = []
    for v in ret['verts']:
        x, y, z = v.co.x, v.co.z, -v.co.y
        py, pz = y * r * 1.12, z * r
        face = pz > 0.02 and py < cut_front
        low = py < cut_low and not (pz < -0.02 and py > cut_low - 0.05)
        top = bald_top and py > 0.08
        if face or low or top:
            kill.append(v)
            continue
        # Short cropped tufts.
        n = 1 + tuft * (math.sin(x * 23 + y * 17) + math.sin(z * 19 - y * 11)) / r
        v.co = T(x * r * 0.96 * n, y * r * 1.12 * n + 0.004, z * r * n - 0.004)
    bmesh.ops.delete(bm, geom=kill, context='VERTS')
    # Give the shell a little thickness so it reads from inside the brim.
    faces = list(bm.faces)
    ret2 = bmesh.ops.solidify(bm, geom=faces, thickness=0.006)
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    return new_object(name, bm, subsurf=1)


def wreath():
    bm = bmesh.new()
    n = 22
    for i in range(n):
        a = 2 * math.pi * i / n
        if abs(math.sin(a / 2)) < 0.12:  # open at the front
            continue
        cx, cz = math.sin(a) * 0.132, math.cos(a) * 0.132 - 0.006
        vs = ellipsoid(bm, cx, 0.058 + 0.01 * math.sin(a * 3), cz, 0.02, 0.009, 0.034, 8, 6)
        rot = a + 0.6
        for v in vs:
            p = Vector((v.co.x, v.co.z, -v.co.y))
            dx, dz = p.x - cx, p.z - cz
            p.x = cx + dx * math.cos(rot) - dz * math.sin(rot)
            p.z = cz + dx * math.sin(rot) + dz * math.cos(rot)
            v.co = T(p.x, p.y, p.z)
    return new_object('wreath', bm, subsurf=0)


# ---------------------------------------------------------------- body
def torso():
    prof = [
        (0.0, 0.2, 0.155),
        (0.1, 0.212, 0.162),
        (0.22, 0.228, 0.17),
        (0.34, 0.245, 0.165),
        (0.42, 0.262, 0.15),
        (0.48, 0.225, 0.13),
        (0.525, 0.12, 0.09),
        (0.545, 0.06, 0.05),
    ]
    fold = lambda a, y, r: 1 + 0.012 * math.sin(a * 6 + y * 8)
    return rings('torso', prof, segs=24, radial=fold, cap_top=True, subsurf=1)


def skirt(name, length, r0, r1, folds, depth):
    prof = []
    steps = 10
    for i in range(steps + 1):
        t = i / steps
        y = -length * t
        r = r0 + (r1 - r0) * (t ** 0.8)
        prof.append((y, r, r * 0.86))
    def fold(a, y, r):
        t = min(1.0, -y / length)
        return 1 + depth * t * (math.sin(a * folds + t * 1.7) * 0.8 + math.sin(a * (folds + 3) - 0.6) * 0.2)
    return rings(name, prof, segs=36, radial=fold, cap_top=False, subsurf=1)


def hem(name, y, r, folds, depth):
    prof = [(y - 0.012, r * 1.01, r * 0.87), (y + 0.035, r * 1.005, r * 0.865)]
    fold = lambda a, yy, rr: 1 + depth * (math.sin(a * folds + 1.7) * 0.8 + math.sin(a * (folds + 3) - 0.6) * 0.2) + 0.008
    return rings(name, prof, segs=36, radial=fold, cap_top=False, subsurf=0)


def ribbon(name, pts, width, thick, twist=0.0, subsurf=1):
    """A draped band through three-space points (x, y, z)."""
    bm = bmesh.new()
    rows = []
    P = [Vector(p) for p in pts]
    for i, p in enumerate(P):
        a = P[max(i - 1, 0)]
        b = P[min(i + 1, len(P) - 1)]
        tangent = (b - a).normalized()
        out = Vector((p.x, 0, p.z)).normalized() if (p.x or p.z) else Vector((0, 0, 1))
        side = tangent.cross(out).normalized()
        w = width * (1 + 0.15 * math.sin(i * 1.7))
        row = []
        for s in (-1, 1):
            for o in (0, 1):
                q = p + side * (s * w / 2) + out * (o * thick)
                row.append(bm.verts.new(T(q.x, q.y, q.z)))
        rows.append(row)
    for r0, r1 in zip(rows, rows[1:]):
        # outer, inner, and the two edges
        bm.faces.new((r0[1], r0[3], r1[3], r1[1]))
        bm.faces.new((r0[2], r0[0], r1[0], r1[2]))
        bm.faces.new((r0[0], r0[1], r1[1], r1[0]))
        bm.faces.new((r0[3], r0[2], r1[2], r1[3]))
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    return new_object(name, bm, subsurf=subsurf)


def drape():
    # Over the left shoulder, across the chest, down to the right hip, and
    # round the back. Chest group space: waist at y = 0.
    pts = [
        (0.2, 0.3, -0.19),
        (0.24, 0.46, -0.12),
        (0.2, 0.54, 0.0),
        (0.14, 0.47, 0.15),
        (0.04, 0.34, 0.2),
        (-0.08, 0.2, 0.2),
        (-0.19, 0.07, 0.16),
        (-0.24, -0.02, 0.02),
        (-0.2, 0.0, -0.13),
    ]
    return ribbon('drape', pts, 0.15, 0.025)


def drape_border():
    pts = [
        (0.2, 0.52, 0.1),
        (0.17, 0.49, 0.17),
        (0.07, 0.36, 0.225),
        (-0.06, 0.22, 0.225),
        (-0.18, 0.09, 0.185),
        (-0.26, -0.01, 0.03),
    ]
    return ribbon('drapeBorder', pts, 0.03, 0.02, subsurf=0)


def stole():
    pts = [(-0.12, 0.1, 0.16), (-0.13, 0.3, 0.17), (-0.1, 0.5, 0.1), (0.0, 0.55, 0.0), (0.1, 0.5, 0.1), (0.13, 0.3, 0.17), (0.12, 0.1, 0.16)]
    return ribbon('stole', pts, 0.08, 0.02)


def cloak():
    prof = []
    for i in range(9):
        t = i / 8
        y = 0.02 - 0.68 * t
        r = 0.27 + 0.1 * t
        prof.append((y, r, r * 0.8))
    fold = lambda a, y, r: 1 + 0.05 * min(1, -y / 0.6) * math.sin(a * 6)
    return rings('cloak', prof, segs=20, radial=fold, cap_top=False, subsurf=1, arc=(math.pi * 0.62, math.pi * 1.38))


def hand():
    bm = bmesh.new()
    ellipsoid(bm, 0, -0.01, 0.005, 0.042, 0.06, 0.03, 12, 10)
    # thumb, toward the front
    ellipsoid(bm, 0.0, 0.0, 0.035, 0.018, 0.034, 0.016, 8, 6)
    return new_object('hand', bm, subsurf=1)


def sandal():
    bm = bmesh.new()
    # sole
    ret = bmesh.ops.create_cube(bm, size=1.0)
    for v in ret['verts']:
        x, y, z = v.co.x, v.co.z, -v.co.y
        v.co = T(x * 0.1, -0.055 + y * 0.018, 0.05 + z * 0.23)
    # foot on top
    ellipsoid(bm, 0, -0.03, 0.06, 0.045, 0.03, 0.105, 12, 8)
    return new_object('foot', bm, subsurf=1)


def limb(name, r0, r1, length, top=0.02):
    prof = []
    for i in range(7):
        t = i / 6
        y = top - (length + top) * t
        r = r0 + (r1 - r0) * t
        # a gentle muscle swell
        r *= 1 + 0.08 * math.sin(t * math.pi)
        prof.append((y, r, r * 0.92))
    return rings(name, prof, segs=12, cap_top=True, cap_bottom=True, subsurf=1)


def sleeve():
    prof = [(0.03, 0.08, 0.075), (-0.06, 0.082, 0.078), (-0.15, 0.086, 0.08)]
    fold = lambda a, y, r: 1 + 0.05 * math.sin(a * 5) * min(1, -y / 0.15)
    return rings('sleeve', prof, segs=16, radial=fold, cap_top=False, subsurf=1)


def neck():
    return limb('neck', 0.056, 0.064, 0.06, top=0.06)


head()
eyes()
brows()
hair('hair')
hair('hairGrey', tuft=0.006)
hair('hairFringe', cut_low=-0.07, bald_top=True)
wreath()
torso()
skirt('tunic', 0.47, 0.205, 0.305, 9, 0.05)
hem('hemTunic', -0.47, 0.305, 9, 0.05)
skirt('toga', 0.9, 0.205, 0.33, 7, 0.07)
hem('hemToga', -0.9, 0.33, 7, 0.07)
drape()
drape_border()
stole()
cloak()
hand()
sandal()
limb('upperArm', 0.058, 0.05, 0.28)
limb('foreArm', 0.05, 0.042, 0.26)
limb('thigh', 0.085, 0.066, 0.44)
limb('shin', 0.064, 0.05, 0.43)
sleeve()
neck()

os.makedirs(os.path.dirname(OUT), exist_ok=True)
bpy.ops.export_scene.gltf(
    filepath=OUT,
    export_format='GLB',
    export_yup=True,
    export_apply=True,
    export_materials='NONE',
    export_normals=True,
    export_texcoords=False,
)
print('wrote', OUT, os.path.getsize(OUT))
