/// Declares a fieldless enum with a 1:1 variant ↔ string table.
///
/// `Variant => "literal"` form: the trailing `const fn as_str;` and optional `fn parse;` lines
/// generate those methods with the given visibility; a trailing `const ALL;` (after `as_str`
/// only) adds `ALL` in declaration order.
///
/// `Variant = index => "literal"` form (metric labels): generates `COUNT`, `index`, `as_str` and
/// `Display` at the enum's visibility, plus `ALL` in declaration order when followed by
/// `const ALL;`.
macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $str:literal),+ $(,)?
        }
        $as_str_vis:vis const fn as_str;
    ) => {
        $(#[$meta])*
        $vis enum $name {
            $($(#[$variant_meta])* $variant),+
        }

        impl $name {
            $as_str_vis const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $str),+
                }
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $str:literal),+ $(,)?
        }
        $as_str_vis:vis const fn as_str;
        $parse_vis:vis fn parse;
    ) => {
        str_enum! {
            $(#[$meta])*
            $vis enum $name {
                $($(#[$variant_meta])* $variant => $str),+
            }
            $as_str_vis const fn as_str;
        }

        impl $name {
            $parse_vis fn parse(value: &str) -> Option<Self> {
                match value {
                    $($str => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($variant:ident = $index:literal => $str:literal),+ $(,)?
        }
    ) => {
        str_enum! {
            $(#[$meta])*
            $vis enum $name {
                $($variant => $str),+
            }
            $vis const fn as_str;
        }

        impl $name {
            $vis const COUNT: usize = [$($str),+].len();

            $vis const fn index(self) -> usize {
                match self {
                    $(Self::$variant => $index),+
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($variant:ident = $index:literal => $str:literal),+ $(,)?
        }
        const ALL;
    ) => {
        str_enum! {
            $(#[$meta])*
            $vis enum $name {
                $($variant = $index => $str),+
            }
        }

        impl $name {
            $vis const ALL: [Self; Self::COUNT] = [$(Self::$variant),+];
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $str:literal),+ $(,)?
        }
        $as_str_vis:vis const fn as_str;
        const ALL;
    ) => {
        str_enum! {
            $(#[$meta])*
            $vis enum $name {
                $($(#[$variant_meta])* $variant => $str),+
            }
            $as_str_vis const fn as_str;
        }

        impl $name {
            $as_str_vis const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

/// Trivial `&self` field getters, one `fn` per entry, for use inside an `impl` block.
///
/// - `vis name: &T` → `&self.name` (deref coercion covers `&str`, `&[T]`, `&Path`).
/// - `vis name: T = method` → `self.name.method()` (`as_ref`, `as_deref`, `clone`).
/// - `vis name: T` → `self.name` (copy).
///
/// `vis name(field): …` reads `field` instead of `name`. Attributes and doc comments on an entry
/// are kept on its method.
macro_rules! getters {
    () => {};
    (
        $(#[$meta:meta])* $vis:vis $name:ident($field:ident): $ty:ty = $method:ident
        $(, $($rest:tt)*)?
    ) => {
        $(#[$meta])*
        $vis fn $name(&self) -> $ty {
            self.$field.$method()
        }
        $(getters! { $($rest)* })?
    };
    (
        $(#[$meta:meta])* $vis:vis $name:ident($field:ident): &$ty:ty
        $(, $($rest:tt)*)?
    ) => {
        $(#[$meta])*
        $vis fn $name(&self) -> &$ty {
            &self.$field
        }
        $(getters! { $($rest)* })?
    };
    (
        $(#[$meta:meta])* $vis:vis $name:ident($field:ident): $ty:ty
        $(, $($rest:tt)*)?
    ) => {
        $(#[$meta])*
        $vis fn $name(&self) -> $ty {
            self.$field
        }
        $(getters! { $($rest)* })?
    };
    ($(#[$meta:meta])* $vis:vis $name:ident: $($rest:tt)*) => {
        getters! { $(#[$meta])* $vis $name($name): $($rest)* }
    };
}
