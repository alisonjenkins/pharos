//! Spike 044: write JVM class files for the Android entry points that must be
//! Java classes (a `Service` named in the manifest, a `MediaSession.Callback`
//! subclass), with no Java or Kotlin source anywhere.
//!
//! Every generated method is one of three shapes:
//! - a no-argument constructor that calls the superclass constructor,
//! - a static initialiser that runs `System.loadLibrary(lib)`,
//! - a `native` method (Rust implements it through JNI), or
//! - a "super, then native" method: it calls the superclass method with the
//!   same arguments and then a private native hook. Lifecycle methods such
//!   as `Service.onCreate` need this, because the framework's own
//!   implementation must still run.
//!
//! The code has no branches, so class version 52 needs no `StackMapTable`.

use std::collections::HashMap;

/// One class to generate. Names use JVM internal form (`a/b/C`, `a/b/C$D`).
#[derive(Debug, Clone)]
pub struct ClassSpec {
    pub name: String,
    pub superclass: String,
    /// Library loaded in `<clinit>`, e.g. `pharos_auto` for `libpharos_auto.so`.
    pub load_library: Option<String>,
    /// Interfaces the class declares it implements, e.g. a listener.
    pub interfaces: Vec<String>,
    pub methods: Vec<Method>,
}

#[derive(Debug, Clone)]
pub enum Method {
    /// `public native <name><descriptor>`.
    Native { name: String, descriptor: String },
    /// `public <name>` calls `super.<name>(args)` then `this.<hook>(args)`,
    /// where `<hook>` is `private native` with the same descriptor. Void only.
    SuperThenNative { name: String, descriptor: String, hook: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum GenError {
    BadDescriptor(String),
    NotVoid { method: String, descriptor: String },
    TooBig(&'static str),
}

impl std::fmt::Display for GenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenError::BadDescriptor(d) => write!(f, "bad method descriptor {d:?}"),
            GenError::NotVoid { method, descriptor } => write!(
                f,
                "super-then-native method {method} must return void, descriptor is {descriptor:?}"
            ),
            GenError::TooBig(what) => write!(f, "{what} exceeds the class file limit"),
        }
    }
}

impl std::error::Error for GenError {}

const ACC_PUBLIC: u16 = 0x0001;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_STATIC: u16 = 0x0008;
const ACC_SUPER: u16 = 0x0020;
const ACC_NATIVE: u16 = 0x0100;

/// Argument kinds, enough to pick the load opcode and the slot count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arg {
    Int,
    Long,
    Float,
    Double,
    Ref,
}

impl Arg {
    fn slots(self) -> u16 {
        match self {
            Arg::Long | Arg::Double => 2,
            _ => 1,
        }
    }
    fn load_opcode(self) -> u8 {
        match self {
            Arg::Int => 0x15,    // iload
            Arg::Long => 0x16,   // lload
            Arg::Float => 0x17,  // fload
            Arg::Double => 0x18, // dload
            Arg::Ref => 0x19,    // aload
        }
    }
}

/// Parses `(args)ret` into the argument kinds and the return descriptor.
fn parse_descriptor(d: &str) -> Result<(Vec<Arg>, &str), GenError> {
    let bad = || GenError::BadDescriptor(d.to_owned());
    let rest = d.strip_prefix('(').ok_or_else(bad)?;
    let close = rest.find(')').ok_or_else(bad)?;
    let (params, ret) = (&rest[..close], &rest[close + 1..]);
    if ret.is_empty() {
        return Err(bad());
    }
    let bytes = params.as_bytes();
    let mut args = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let mut array = false;
        while bytes.get(i) == Some(&b'[') {
            array = true;
            i += 1;
        }
        let c = *bytes.get(i).ok_or_else(bad)?;
        let kind = match c {
            b'L' => {
                let end = params[i..].find(';').ok_or_else(bad)?;
                i += end;
                Arg::Ref
            }
            b'Z' | b'B' | b'C' | b'S' | b'I' => Arg::Int,
            b'J' => Arg::Long,
            b'F' => Arg::Float,
            b'D' => Arg::Double,
            _ => return Err(bad()),
        };
        args.push(if array { Arg::Ref } else { kind });
        i += 1;
    }
    Ok((args, ret))
}

#[derive(Default)]
struct Pool {
    bytes: Vec<u8>,
    count: u16,
    index: HashMap<Vec<u8>, u16>,
}

impl Pool {
    fn add(&mut self, entry: Vec<u8>) -> Result<u16, GenError> {
        if let Some(&i) = self.index.get(&entry) {
            return Ok(i);
        }
        let i = self.count.checked_add(1).ok_or(GenError::TooBig("constant pool"))?;
        self.bytes.extend_from_slice(&entry);
        self.index.insert(entry, i);
        self.count = i;
        Ok(i)
    }
    fn utf8(&mut self, s: &str) -> Result<u16, GenError> {
        // Every name here is ASCII, where modified UTF-8 equals UTF-8.
        let len = u16::try_from(s.len()).map_err(|_| GenError::TooBig("string"))?;
        let mut e = vec![1];
        e.extend_from_slice(&len.to_be_bytes());
        e.extend_from_slice(s.as_bytes());
        self.add(e)
    }
    fn class(&mut self, name: &str) -> Result<u16, GenError> {
        let n = self.utf8(name)?;
        let mut e = vec![7];
        e.extend_from_slice(&n.to_be_bytes());
        self.add(e)
    }
    fn string(&mut self, s: &str) -> Result<u16, GenError> {
        let n = self.utf8(s)?;
        let mut e = vec![8];
        e.extend_from_slice(&n.to_be_bytes());
        self.add(e)
    }
    fn method_ref(&mut self, class: &str, name: &str, desc: &str) -> Result<u16, GenError> {
        let c = self.class(class)?;
        let n = self.utf8(name)?;
        let t = self.utf8(desc)?;
        let mut nt = vec![12];
        nt.extend_from_slice(&n.to_be_bytes());
        nt.extend_from_slice(&t.to_be_bytes());
        let nt = self.add(nt)?;
        let mut e = vec![10];
        e.extend_from_slice(&c.to_be_bytes());
        e.extend_from_slice(&nt.to_be_bytes());
        self.add(e)
    }
}

struct MethodOut {
    access: u16,
    name: u16,
    descriptor: u16,
    /// (max_stack, max_locals, code)
    code: Option<(u16, u16, Vec<u8>)>,
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Writes the class file bytes for `spec`.
pub fn generate(spec: &ClassSpec) -> Result<Vec<u8>, GenError> {
    let mut pool = Pool::default();
    let this = pool.class(&spec.name)?;
    let sup = pool.class(&spec.superclass)?;
    let interfaces = spec
        .interfaces
        .iter()
        .map(|i| pool.class(i))
        .collect::<Result<Vec<_>, _>>()?;
    let code_attr = pool.utf8("Code")?;
    let mut methods = Vec::new();

    // Constructor: aload_0; invokespecial super.<init>()V; return.
    let super_init = pool.method_ref(&spec.superclass, "<init>", "()V")?;
    let mut code = vec![0x2a, 0xb7];
    push_u16(&mut code, super_init);
    code.push(0xb1);
    methods.push(MethodOut {
        access: ACC_PUBLIC,
        name: pool.utf8("<init>")?,
        descriptor: pool.utf8("()V")?,
        code: Some((1, 1, code)),
    });

    if let Some(lib) = &spec.load_library {
        // ldc_w "lib"; invokestatic System.loadLibrary; return.
        let s = pool.string(lib)?;
        let load = pool.method_ref("java/lang/System", "loadLibrary", "(Ljava/lang/String;)V")?;
        let mut code = vec![0x13];
        push_u16(&mut code, s);
        code.push(0xb8);
        push_u16(&mut code, load);
        code.push(0xb1);
        methods.push(MethodOut {
            access: ACC_STATIC,
            name: pool.utf8("<clinit>")?,
            descriptor: pool.utf8("()V")?,
            code: Some((1, 0, code)),
        });
    }

    for m in &spec.methods {
        match m {
            Method::Native { name, descriptor } => {
                parse_descriptor(descriptor)?;
                methods.push(MethodOut {
                    access: ACC_PUBLIC | ACC_NATIVE,
                    name: pool.utf8(name)?,
                    descriptor: pool.utf8(descriptor)?,
                    code: None,
                });
            }
            Method::SuperThenNative { name, descriptor, hook } => {
                let (args, ret) = parse_descriptor(descriptor)?;
                if ret != "V" {
                    return Err(GenError::NotVoid {
                        method: name.clone(),
                        descriptor: descriptor.clone(),
                    });
                }
                let super_ref = pool.method_ref(&spec.superclass, name, descriptor)?;
                let hook_ref = pool.method_ref(&spec.name, hook, descriptor)?;
                let arg_slots: u16 = args.iter().map(|a| a.slots()).sum();
                let mut code = Vec::new();
                for (op, target) in [(0xb7u8, super_ref), (0xb7u8, hook_ref)] {
                    // invokespecial for both: a private method is invoked
                    // with invokespecial in class version 52.
                    code.push(0x2a); // aload_0
                    let mut slot: u16 = 1;
                    for a in &args {
                        let s = u8::try_from(slot).map_err(|_| GenError::TooBig("locals"))?;
                        code.push(a.load_opcode());
                        code.push(s);
                        slot += a.slots();
                    }
                    code.push(op);
                    push_u16(&mut code, target);
                }
                code.push(0xb1);
                let locals = 1 + arg_slots;
                methods.push(MethodOut {
                    access: ACC_PUBLIC,
                    name: pool.utf8(name)?,
                    descriptor: pool.utf8(descriptor)?,
                    code: Some((locals, locals, code)),
                });
                methods.push(MethodOut {
                    access: ACC_PRIVATE | ACC_NATIVE,
                    name: pool.utf8(hook)?,
                    descriptor: pool.utf8(descriptor)?,
                    code: None,
                });
            }
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    push_u16(&mut out, 0); // minor
    push_u16(&mut out, 52); // major: Java 8, which d8 accepts
    push_u16(&mut out, pool.count + 1);
    out.extend_from_slice(&pool.bytes);
    push_u16(&mut out, ACC_PUBLIC | ACC_SUPER);
    push_u16(&mut out, this);
    push_u16(&mut out, sup);
    push_u16(&mut out, u16::try_from(interfaces.len()).map_err(|_| GenError::TooBig("interfaces"))?);
    for i in interfaces {
        push_u16(&mut out, i);
    }
    push_u16(&mut out, 0); // fields
    push_u16(&mut out, u16::try_from(methods.len()).map_err(|_| GenError::TooBig("methods"))?);
    for m in methods {
        push_u16(&mut out, m.access);
        push_u16(&mut out, m.name);
        push_u16(&mut out, m.descriptor);
        match m.code {
            None => push_u16(&mut out, 0),
            Some((max_stack, max_locals, code)) => {
                push_u16(&mut out, 1);
                push_u16(&mut out, code_attr);
                let code_len = u32::try_from(code.len()).map_err(|_| GenError::TooBig("code"))?;
                // max_stack(2) max_locals(2) code_length(4) code ex_table_len(2) attrs(2)
                out.extend_from_slice(&(12 + code_len).to_be_bytes());
                push_u16(&mut out, max_stack);
                push_u16(&mut out, max_locals);
                out.extend_from_slice(&code_len.to_be_bytes());
                out.extend_from_slice(&code);
                push_u16(&mut out, 0);
                push_u16(&mut out, 0);
            }
        }
    }
    push_u16(&mut out, 0); // class attributes
    Ok(out)
}

/// The two classes the Android Auto spike needs.
pub fn android_auto_classes() -> Vec<ClassSpec> {
    const PKG: &str = "com/alisonjenkins/pharos/auto";
    let native = |name: &str, descriptor: &str| Method::Native {
        name: name.into(),
        descriptor: descriptor.into(),
    };
    vec![
        ClassSpec {
            name: format!("{PKG}/PharosMediaService"),
            superclass: "android/service/media/MediaBrowserService".into(),
            load_library: Some("pharos_auto".into()),
            interfaces: Vec::new(),
            methods: vec![
                Method::SuperThenNative {
                    name: "onCreate".into(),
                    descriptor: "()V".into(),
                    hook: "nativeOnCreate".into(),
                },
                Method::SuperThenNative {
                    name: "onDestroy".into(),
                    descriptor: "()V".into(),
                    hook: "nativeOnDestroy".into(),
                },
                native(
                    "onGetRoot",
                    "(Ljava/lang/String;ILandroid/os/Bundle;)Landroid/service/media/MediaBrowserService$BrowserRoot;",
                ),
                native(
                    "onLoadChildren",
                    "(Ljava/lang/String;Landroid/service/media/MediaBrowserService$Result;)V",
                ),
                native(
                    "onSearch",
                    "(Ljava/lang/String;Landroid/os/Bundle;Landroid/service/media/MediaBrowserService$Result;)V",
                ),
            ],
        },
        ClassSpec {
            name: format!("{PKG}/PharosSessionCallback"),
            superclass: "android/media/session/MediaSession$Callback".into(),
            load_library: None,
            interfaces: Vec::new(),
            methods: vec![
                native("onPlay", "()V"),
                native("onPause", "()V"),
                native("onStop", "()V"),
                native("onSkipToNext", "()V"),
                native("onSkipToPrevious", "()V"),
                native("onSeekTo", "(J)V"),
                native("onPlayFromMediaId", "(Ljava/lang/String;Landroid/os/Bundle;)V"),
                // Gemini / Assistant voice requests ("play X on Pharos").
                native("onPlayFromSearch", "(Ljava/lang/String;Landroid/os/Bundle;)V"),
            ],
        },
        // The phone-side voice entry point: the assistant lists an app as
        // able to take "play X on <app>" only if an activity handles
        // MEDIA_PLAY_FROM_SEARCH. It has no UI; Rust reads the query and
        // finishes it.
        ClassSpec {
            name: format!("{PKG}/PharosSearchActivity"),
            superclass: "android/app/Activity".into(),
            load_library: Some("pharos_auto".into()),
            interfaces: Vec::new(),
            methods: vec![Method::SuperThenNative {
                name: "onCreate".into(),
                descriptor: "(Landroid/os/Bundle;)V".into(),
                hook: "nativeOnCreate".into(),
            }],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_slots() {
        let (args, ret) = parse_descriptor("(JLjava/lang/String;[I[[Ljava/lang/Object;DZ)V").unwrap();
        assert_eq!(args, vec![Arg::Long, Arg::Ref, Arg::Ref, Arg::Ref, Arg::Double, Arg::Int]);
        assert_eq!(ret, "V");
        assert!(parse_descriptor("()").is_err());
        assert!(parse_descriptor("(Q)V").is_err());
        assert!(parse_descriptor("(Ljava/lang/String)V").is_err());
    }

    #[test]
    fn super_then_native_must_be_void() {
        let spec = ClassSpec {
            name: "a/B".into(),
            superclass: "java/lang/Object".into(),
            load_library: None,
            interfaces: Vec::new(),
            methods: vec![Method::SuperThenNative {
                name: "x".into(),
                descriptor: "()I".into(),
                hook: "y".into(),
            }],
        };
        assert!(matches!(generate(&spec), Err(GenError::NotVoid { .. })));
    }

    #[test]
    fn classes_start_with_magic() {
        for spec in android_auto_classes() {
            let bytes = generate(&spec).unwrap();
            assert_eq!(&bytes[..4], &[0xCA, 0xFE, 0xBA, 0xBE], "{}", spec.name);
        }
    }
}
