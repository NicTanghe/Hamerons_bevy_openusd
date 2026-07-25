//! Author-back (PLAN P2): ECS component → USD opinions.
//!
//! The write direction of the reflect route. Where [`crate::route::reflect`]
//! projects `bevy:` attributes onto components, this reads a component back
//! and authors its fields as `bevy:<Type>:<field>` attributes into the stage's
//! current edit target. This is the round-trip BSN structurally can't do:
//! spawned entities remain views of the stage, and edits flow back into
//! layers that persist and export.
//!
//! USD is still the source of truth. After authoring, the commit fires the
//! change sink and the reflect route re-projects — writing the same value it
//! just read, so the round-trip is idempotent (see the echo guard on
//! [`crate::live::LiveStage`]).

use bevy::ecs::reflect::{AppTypeRegistry, ReflectComponent};
use bevy::prelude::*;
use bevy::reflect::enums::VariantType;
use bevy::reflect::{PartialReflect, ReflectRef, TypeRegistry};
use openusd::sdf::Value;
use openusd::usd::Stage;

use crate::authoring::set_attribute;

type Result<T> = anyhow::Result<T>;

/// A reflected leaf value as a USD `(value, typeName)` pair, or `None` for a
/// field type we don't author (mirrors the forward route's coverage).
fn value_of(field: &dyn PartialReflect) -> Option<(Value, &'static str)> {
    if let Some(x) = field.try_downcast_ref::<f64>() {
        return Some((Value::Double(*x), "double"));
    }
    if let Some(x) = field.try_downcast_ref::<f32>() {
        return Some((Value::Float(*x), "float"));
    }
    if let Some(x) = field.try_downcast_ref::<i32>() {
        return Some((Value::Int(*x), "int"));
    }
    if let Some(x) = field.try_downcast_ref::<i64>() {
        return Some((Value::Int64(*x), "int64"));
    }
    if let Some(x) = field.try_downcast_ref::<u32>() {
        return Some((Value::Uint(*x), "uint"));
    }
    if let Some(x) = field.try_downcast_ref::<u64>() {
        return Some((Value::Uint64(*x), "uint64"));
    }
    if let Some(x) = field.try_downcast_ref::<usize>() {
        return Some((Value::Uint64(*x as u64), "uint64"));
    }
    if let Some(x) = field.try_downcast_ref::<bool>() {
        return Some((Value::Bool(*x), "bool"));
    }
    if let Some(x) = field.try_downcast_ref::<String>() {
        return Some((Value::String(x.clone()), "string"));
    }
    if let Some(x) = field.try_downcast_ref::<Vec2>() {
        return Some((Value::Vec2f([x.x, x.y].into()), "float2"));
    }
    if let Some(x) = field.try_downcast_ref::<Vec3>() {
        return Some((Value::Vec3f([x.x, x.y, x.z].into()), "float3"));
    }
    if let Some(x) = field.try_downcast_ref::<Vec4>() {
        return Some((Value::Vec4f([x.x, x.y, x.z, x.w].into()), "float4"));
    }
    if let Some(x) = field.try_downcast_ref::<Quat>() {
        // gf Quatf fields are (w, x, y, z); build them explicitly rather than
        // via `From<[f32;4]>` (which is real-first and would scramble xyzw).
        return Some((
            Value::Quatf(openusd::gf::Quatf {
                w: x.w,
                x: x.x,
                y: x.y,
                z: x.z,
            }),
            "quatf",
        ));
    }
    if let Some(c) = field.try_downcast_ref::<Color>() {
        // Author as linear `color4f` (round-trips with the forward route's
        // linear mapping, preserving alpha).
        let l = c.to_linear();
        return Some((
            Value::Vec4f([l.red, l.green, l.blue, l.alpha].into()),
            "color4f",
        ));
    }
    // Array fields.
    if let Some(x) = field.try_downcast_ref::<Vec<f32>>() {
        return Some((Value::FloatVec(x.clone()), "float[]"));
    }
    if let Some(x) = field.try_downcast_ref::<Vec<i32>>() {
        return Some((Value::IntVec(x.clone()), "int[]"));
    }
    if let Some(x) = field.try_downcast_ref::<Vec<String>>() {
        return Some((Value::StringVec(x.clone()), "string[]"));
    }
    if let Some(x) = field.try_downcast_ref::<Vec<Vec3>>() {
        let v = x.iter().map(|p| [p.x, p.y, p.z].into()).collect();
        return Some((Value::Vec3fVec(v), "float3[]"));
    }
    // Option<T>: `Some` authors the inner value; `None` authors nothing.
    if let Some(o) = field.try_downcast_ref::<Option<f32>>() {
        return (*o).map(|x| (Value::Float(x), "float"));
    }
    if let Some(o) = field.try_downcast_ref::<Option<f64>>() {
        return (*o).map(|x| (Value::Double(x), "double"));
    }
    if let Some(o) = field.try_downcast_ref::<Option<i32>>() {
        return (*o).map(|x| (Value::Int(x), "int"));
    }
    if let Some(o) = field.try_downcast_ref::<Option<bool>>() {
        return (*o).map(|x| (Value::Bool(x), "bool"));
    }
    if let Some(o) = field.try_downcast_ref::<Option<String>>() {
        return o.as_ref().map(|x| (Value::String(x.clone()), "string"));
    }
    if let Some(o) = field.try_downcast_ref::<Option<Vec3>>() {
        return o.map(|v| (Value::Vec3f([v.x, v.y, v.z].into()), "float3"));
    }
    // Unit-variant enums author as a token naming the variant.
    if let ReflectRef::Enum(e) = field.reflect_ref()
        && e.variant_type() == VariantType::Unit
    {
        return Some((Value::Token(e.variant_name().into()), "token"));
    }
    None
}

/// Walk a reflected value into leaf `(field_path, value, typeName)` triples,
/// descending named struct fields with `:` (the same namespacing the forward
/// route reads). A leaf is any value [`value_of`] can encode; a whole vector /
/// quaternion is one leaf (tried before descending its `x`/`y`/`z` fields).
fn walk(field: &dyn PartialReflect, prefix: &str, out: &mut Vec<(String, Value, &'static str)>) {
    if let Some((v, ty)) = value_of(field) {
        out.push((prefix.to_string(), v, ty));
        return;
    }
    match field.reflect_ref() {
        ReflectRef::Struct(s) => {
            for i in 0..s.field_len() {
                let Some(name) = s.name_at(i) else { continue };
                let Some(child) = s.field_at(i) else { continue };
                let path = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}:{name}")
                };
                walk(child, &path, out);
            }
        }
        // Tuple structs (incl. newtypes like `struct Level(u32)`) descend by
        // index. USD identifiers can't start with a digit, so the index is
        // authored as `_0`, `_1`, … (decoded back to the reflect path `.0` by
        // the forward route).
        ReflectRef::TupleStruct(ts) => {
            for i in 0..ts.field_len() {
                let Some(child) = ts.field(i) else { continue };
                let path = if prefix.is_empty() {
                    format!("_{i}")
                } else {
                    format!("{prefix}:_{i}")
                };
                walk(child, &path, out);
            }
        }
        // Data-carrying enum variants (PLAN 4c): author the variant name at this
        // path (mirroring the unit case in `value_of`), then descend the active
        // variant's payload — tuple fields as `_i`, struct fields by name — so
        // the forward route reconstructs the variant. (Unit variants never reach
        // here: `value_of` already emitted them as a token.)
        ReflectRef::Enum(e) => {
            // `Option<T>` is a reflect enum too; `value_of` owns it (a `Some`
            // authors its inner value, a `None` authors nothing). Never treat it
            // as a variant token, or a `None` field would leak a `"None"` token.
            if field.reflect_type_path().starts_with("core::option::Option") {
                return;
            }
            out.push((prefix.to_string(), Value::Token(e.variant_name().into()), "token"));
            match e.variant_type() {
                VariantType::Tuple => {
                    for i in 0..e.field_len() {
                        let Some(child) = e.field_at(i) else { continue };
                        let path = if prefix.is_empty() {
                            format!("_{i}")
                        } else {
                            format!("{prefix}:_{i}")
                        };
                        walk(child, &path, out);
                    }
                }
                VariantType::Struct => {
                    for i in 0..e.field_len() {
                        let Some(name) = e.name_at(i) else { continue };
                        let Some(child) = e.field_at(i) else { continue };
                        let path = if prefix.is_empty() {
                            name.to_string()
                        } else {
                            format!("{prefix}:{name}")
                        };
                        walk(child, &path, out);
                    }
                }
                VariantType::Unit => {}
            }
        }
        _ => {}
    }
}

/// Author every encodable field of the reflect component `short_type` on
/// `entity` as `bevy:<short_type>:<field>` attributes on `prim_path`, into the
/// stage's current edit target. Returns the authored attribute names.
///
/// This is the inverse of the reflect route. Pair it with the stage's edit
/// target (session layer for a scratch override, a stronger layer to persist)
/// and, in a live session, the echo guard so the re-projection is swallowed.
pub fn author_component(
    world: &World,
    registry: &TypeRegistry,
    stage: &Stage,
    entity: Entity,
    prim_path: &str,
    short_type: &str,
) -> Result<Vec<String>> {
    let registration = registry
        .get_with_short_type_path(short_type)
        .or_else(|| registry.get_with_type_path(short_type))
        .ok_or_else(|| anyhow::anyhow!("type `{short_type}` is not registered"))?;
    let reflect_component = registration
        .data::<ReflectComponent>()
        .ok_or_else(|| anyhow::anyhow!("type `{short_type}` has no ReflectComponent"))?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| anyhow::anyhow!("entity {entity:?} does not exist"))?;
    let component = reflect_component
        .reflect(entity_ref)
        .ok_or_else(|| anyhow::anyhow!("entity {entity:?} has no `{short_type}` component"))?;

    let mut leaves = Vec::new();
    walk(component.as_partial_reflect(), "", &mut leaves);

    let mut authored = Vec::new();
    for (path, value, type_name) in leaves {
        let attr = format!("bevy:{short_type}:{path}");
        set_attribute(stage, prim_path, &attr, type_name, value)?;
        authored.push(attr);
    }
    Ok(authored)
}

/// Convenience for reading the `AppTypeRegistry` out of the world and authoring
/// `short_type` on `entity` in one call. The registry read-lock is held only
/// for the duration of the author.
pub fn author_component_from_world(
    world: &World,
    stage: &Stage,
    entity: Entity,
    prim_path: &str,
    short_type: &str,
) -> Result<Vec<String>> {
    let app_registry = world
        .get_resource::<AppTypeRegistry>()
        .ok_or_else(|| anyhow::anyhow!("no AppTypeRegistry in world"))?
        .clone();
    let registry = app_registry.read();
    author_component(world, &registry, stage, entity, prim_path, short_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::SchemaRegistry;
    use openusd::usd::Stage;

    #[derive(Component, Reflect, Default, Debug, Clone, PartialEq)]
    #[reflect(Component, Default)]
    struct Health {
        current: f64,
        max: f64,
    }

    #[derive(Component, Reflect, Default, Debug, Clone, PartialEq)]
    #[reflect(Component, Default)]
    struct Placement {
        offset: Vec3,
    }

    fn world_with(register: impl FnOnce(&mut TypeRegistry)) -> World {
        let mut world = World::new();
        let type_registry = AppTypeRegistry::default();
        register(&mut type_registry.write());
        world.insert_resource(type_registry);
        world.insert_resource(SchemaRegistry::builtin());
        world
    }

    /// A component authored back into the stage re-reads (via the reflect
    /// route) into an identical component — a full ECS → USD → ECS round-trip,
    /// the thing BSN cannot do. Also proves the opinions land in an exportable
    /// layer.
    #[test]
    fn author_component_roundtrips_through_usd() {
        let stage = Stage::builder().in_memory("author.usda").unwrap();
        stage
            .define_prim("/Enemy")
            .unwrap()
            .set_type_name("Xform")
            .unwrap();

        // Source world holds the component; author it onto the stage.
        let mut src = world_with(|r| {
            r.register::<Health>();
            r.register::<Placement>();
        });
        let e = src
            .spawn((
                crate::UsdPrimRef::new("/Enemy"),
                Health {
                    current: 30.0,
                    max: 120.0,
                },
                Placement {
                    offset: Vec3::new(1.0, 2.0, 3.0),
                },
            ))
            .id();
        author_component_from_world(&src, &stage, e, "/Enemy", "Health").unwrap();
        author_component_from_world(&src, &stage, e, "/Enemy", "Placement").unwrap();

        // The opinions are really in the layer.
        let text = crate::authoring::export_stage_string(&stage).unwrap();
        assert!(
            text.contains("bevy:Health:max"),
            "authored attr present in exported layer:\n{text}"
        );

        // Re-project the stage into a fresh world → identical components.
        let live = LiveStage::new(stage);
        let mut dst = world_with(|r| {
            r.register::<Health>();
            r.register::<Placement>();
        });
        let mut map = PrimEntities::default();
        project_stage(&mut dst, &live, &mut map);
        let re = map.entity("/Enemy").unwrap();
        assert_eq!(
            dst.get::<Health>(re),
            Some(&Health {
                current: 30.0,
                max: 120.0
            }),
            "Health round-trips ECS → USD → ECS"
        );
        assert_eq!(
            dst.get::<Placement>(re),
            Some(&Placement {
                offset: Vec3::new(1.0, 2.0, 3.0)
            }),
            "nested Vec3 field round-trips as float3"
        );
    }

    #[derive(Reflect, Default, Debug, Clone, PartialEq)]
    enum Mode {
        #[default]
        Idle,
        Run,
    }

    #[derive(Component, Reflect, Default, Debug, Clone, PartialEq)]
    #[reflect(Component, Default)]
    struct Mixed {
        flag: bool,
        count: i32,
        label: String,
        mode: Mode,
        spin: Quat,
    }

    #[derive(Reflect, Default, Debug, Clone, PartialEq)]
    struct Inner {
        a: f32,
        b: f32,
    }

    #[derive(Component, Reflect, Default, Debug, Clone, PartialEq)]
    #[reflect(Component, Default)]
    struct Outer {
        inner: Inner,
        tag: i32,
    }

    #[derive(Component, Reflect, Default, Debug, Clone, PartialEq)]
    #[reflect(Component, Default)]
    struct Tint {
        color: Color,
    }

    #[derive(Component, Reflect, Default, Debug, Clone, PartialEq)]
    #[reflect(Component, Default)]
    struct Opts {
        maybe_hp: Option<f32>,
        maybe_name: Option<String>,
        count: u64,
        idx: usize,
    }

    #[derive(Reflect, Default, Debug, Clone, PartialEq)]
    enum Motion {
        #[default]
        Idle,
        Moving(f32),
        Warp {
            x: i32,
            y: i32,
        },
    }

    #[derive(Component, Reflect, Default, Debug, Clone, PartialEq)]
    #[reflect(Component, Default)]
    struct Mover {
        motion: Motion,
    }

    /// `Option<T>` (Some), `u64`, and `usize` fields round-trip; a `None` option
    /// authors nothing and re-reads as the default.
    #[test]
    fn option_and_wide_int_roundtrip() {
        let value = Opts {
            maybe_hp: Some(12.5),
            maybe_name: Some("boss".into()),
            count: 9_000_000_000,
            idx: 7,
        };
        let back = roundtrip(|r| r.register::<Opts>(), value.clone());
        assert_eq!(back, value, "Some options + u64/usize round-trip");

        // A None option authors nothing → re-reads as the default (None).
        let none = Opts {
            maybe_hp: None,
            maybe_name: None,
            count: 1,
            idx: 2,
        };
        let back = roundtrip(|r| r.register::<Opts>(), none.clone());
        assert_eq!(back, none, "None options stay None through the round-trip");
    }

    #[test]
    fn data_enum_roundtrip() {
        // Unit variant.
        let idle = Mover { motion: Motion::Idle };
        assert_eq!(
            roundtrip(|r| r.register::<Mover>(), idle.clone()),
            idle,
            "unit variant round-trips as a bare token"
        );
        // Tuple variant carries its payload.
        let moving = Mover {
            motion: Motion::Moving(4.5),
        };
        assert_eq!(
            roundtrip(|r| r.register::<Mover>(), moving.clone()),
            moving,
            "tuple-variant payload round-trips"
        );
        // Struct variant carries its named fields.
        let warp = Mover {
            motion: Motion::Warp { x: 3, y: 7 },
        };
        assert_eq!(
            roundtrip(|r| r.register::<Mover>(), warp.clone()),
            warp,
            "struct-variant payload round-trips"
        );
    }

    /// A `Color` field round-trips as linear `color4f` (alpha preserved).
    #[test]
    fn color_roundtrip() {
        let value = Tint {
            color: Color::linear_rgba(0.1, 0.2, 0.3, 0.4),
        };
        let back = roundtrip(|r| r.register::<Tint>(), value.clone());
        assert_eq!(back, value, "Color survives ECS → USD → ECS in linear space");
    }

    #[derive(Component, Reflect, Default, Debug, Clone, PartialEq)]
    #[reflect(Component, Default)]
    struct Level(u32);

    /// A newtype / tuple-struct component authors its field by index (`:0`).
    #[test]
    fn newtype_roundtrip() {
        let back = roundtrip(|r| r.register::<Level>(), Level(42));
        assert_eq!(back, Level(42), "tuple-struct field `.0` round-trips");
    }

    #[derive(Component, Reflect, Default, Debug, Clone, PartialEq)]
    #[reflect(Component, Default)]
    struct Path {
        waypoints: Vec<Vec3>,
        weights: Vec<f32>,
        labels: Vec<String>,
    }

    /// Array fields round-trip through USD array values.
    #[test]
    fn array_fields_roundtrip() {
        let value = Path {
            waypoints: vec![Vec3::new(0.0, 1.0, 2.0), Vec3::new(3.0, 4.0, 5.0)],
            weights: vec![0.5, 1.5, 2.5],
            labels: vec!["a".into(), "b".into()],
        };
        let back = roundtrip(|r| r.register::<Path>(), value.clone());
        assert_eq!(back, value, "Vec<Vec3>/Vec<f32>/Vec<String> round-trip");
    }

    fn roundtrip<C: Component + PartialReflect + Clone + PartialEq + std::fmt::Debug>(
        register: impl Fn(&mut TypeRegistry) + Copy,
        value: C,
    ) -> C {
        let stage = Stage::builder().in_memory("rt.usda").unwrap();
        stage.define_prim("/P").unwrap();
        let short = std::any::type_name::<C>().rsplit("::").next().unwrap();

        let mut src = world_with(register);
        let e = src.spawn((crate::UsdPrimRef::new("/P"), value)).id();
        author_component_from_world(&src, &stage, e, "/P", short).unwrap();

        let live = LiveStage::new(stage);
        let mut dst = world_with(register);
        let mut map = PrimEntities::default();
        project_stage(&mut dst, &live, &mut map);
        let re = map.entity("/P").unwrap();
        dst.get::<C>(re).cloned().expect("component re-projected")
    }

    /// bool / int / String / unit-enum / quaternion all survive ECS → USD →
    /// ECS. The quaternion case guards the gf real-first ordering bug.
    #[test]
    fn scalar_enum_quat_roundtrip() {
        let value = Mixed {
            flag: true,
            count: -7,
            label: "boss".into(),
            mode: Mode::Run,
            spin: Quat::from_xyzw(0.1, 0.2, 0.3, 0.7),
        };
        let back = roundtrip(
            |r| {
                r.register::<Mixed>();
                r.register::<Mode>();
            },
            value.clone(),
        );
        assert_eq!(back, value, "all field kinds round-trip, quat unscrambled");
    }

    /// Nested struct fields round-trip through `:`-namespaced attribute paths in
    /// both directions.
    #[test]
    fn nested_struct_roundtrip() {
        let value = Outer {
            inner: Inner { a: 1.5, b: -2.5 },
            tag: 9,
        };
        let back = roundtrip(
            |r| {
                r.register::<Outer>();
                r.register::<Inner>();
            },
            value.clone(),
        );
        assert_eq!(back, value, "nested `inner:a`/`inner:b` round-trip");
    }

    #[test]
    fn author_errors_are_graceful() {
        let stage = Stage::builder().in_memory("err.usda").unwrap();
        stage.define_prim("/P").unwrap();
        let mut world = world_with(|r| r.register::<Health>());
        let e = world.spawn(crate::UsdPrimRef::new("/P")).id();

        // Unregistered type.
        assert!(
            author_component_from_world(&world, &stage, e, "/P", "Nope").is_err(),
            "unregistered type errors, not panics"
        );
        // Registered but entity lacks the component.
        assert!(
            author_component_from_world(&world, &stage, e, "/P", "Health").is_err(),
            "missing component errors"
        );
    }

    /// The reflect route must not panic when a stage carries `bevy:` opinions
    /// but the world has no `AppTypeRegistry` (a bare headless world).
    #[test]
    fn no_type_registry_is_graceful() {
        let stage = Stage::builder().in_memory("noreg.usda").unwrap();
        stage.define_prim("/P").unwrap();
        stage
            .create_attribute("/P.bevy:Health:max", "double")
            .unwrap()
            .set(Value::Double(1.0))
            .unwrap();
        let live = LiveStage::new(stage);
        let mut world = World::new(); // no AppTypeRegistry, no SchemaRegistry
        let mut map = PrimEntities::default();
        // Should log a warning and carry on, not panic.
        project_stage(&mut world, &live, &mut map);
        assert!(map.entity("/P").is_some(), "prim still projected");
    }
}
