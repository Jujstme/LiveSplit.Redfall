//! Support for attaching to games using the Unreal Engine
//!
//! This version has been tailored specifically to work on Redfall,
//! it won't work on other games!

use core::{
    array,
    cell::RefCell,
    iter::{self, FusedIterator},
    mem::size_of,
};

use bytemuck::CheckedBitPattern;

use asr::{
    file_format::pe, signature::Signature, string::ArrayCString, Address, PointerSize, Process,
};

const CSTR: usize = 128;

/// Represents access to a Unreal Engine game.
///
/// This struct gives immediate access to 2 important structs present in every UE game:
/// - GEngine: a static object that persists throughout the process' lifetime
/// - GWorld: a pointer to the currently loaded UWorld object
pub struct Module {
    pointer_size: PointerSize,
    offsets: &'static Offsets,
    g_engine: Address,
    fname_base: Address,
}

impl Module {
    /// Tries attaching to a UE game. The UE version needs to be correct for this
    /// function to work.
    pub fn attach(process: &Process, main_module_address: Address) -> Option<Self> {
        let pointer_size = pe::MachineType::read(process, main_module_address)?.pointer_size()?;
        let offsets = Offsets::new();
        let module_size = pe::read_size_of_image(process, main_module_address)? as u64;
        let module_range = (main_module_address, module_size);

        let g_engine = {
            const GENGINE_1: (Signature<14>, u32) = (
                Signature::new("48 8B 05 ?? ?? ?? ?? 8B 88 ?? ?? ?? ?? 41"),
                3,
            );
            const GENGINE_2: (Signature<7>, u32) = (Signature::new("A8 01 75 ?? 48 C7 05"), 7);

            if let Some(g_engine) = GENGINE_1.0.scan_process_range(process, module_range) {
                let addr = g_engine + GENGINE_1.1;
                addr + 0x4 + process.read::<i32>(addr).ok()?
            } else if let Some(g_engine) = GENGINE_2.0.scan_process_range(process, module_range) {
                let addr = g_engine + GENGINE_2.1;
                addr + 0x8 + process.read::<i32>(addr).ok()?
            } else {
                return None;
            }
        };

        let fname_base = {
            const FNAME_POOL: &[(Signature<13>, u8)] = &[
                (Signature::new("74 09 48 8D 15 ?? ?? ?? ?? EB 16 ?? ??"), 5),
                (Signature::new("89 5C 24 ?? 89 44 24 ?? 74 ?? 48 8D 15"), 13),
                (Signature::new("57 0F B7 F8 74 ?? B8 ?? ?? ?? ?? 8B 44"), 7),
            ];

            let addr = FNAME_POOL.iter().find_map(|(sig, offset)| {
                Some(sig.scan_process_range(process, module_range)? + *offset)
            })?;
            addr + 0x4 + process.read::<i32>(addr).ok()?
        };

        Some(Self {
            pointer_size,
            offsets,
            g_engine,
            fname_base,
        })
    }

    /// Returns the memory pointer to GEngine
    pub const fn g_engine(&self) -> Address {
        self.g_engine
    }

    #[inline]
    const fn size_of_ptr(&self) -> u64 {
        self.pointer_size as u64
    }
}

/// An `UObject` is the base class of every Unreal Engine object,
/// from which every other class in the UE engine inherits from.
///
/// This struct represents a currently running instance of any UE class,
/// from which it's possible to perform introspection in order to return
/// various information, such as the class' `FName`, property names, offsets, etc.
///
// Docs:
// - https://docs.unrealengine.com/4.27/en-US/API/Runtime/CoreUObject/UObject/UObject/
// - https://gist.github.com/apple1417/b23f91f7a9e3b834d6d052d35a0010ff#object-structure
#[derive(Copy, Clone)]
pub struct UObject {
    object: Address,
}

impl UObject {
    /// Returns the underlying class definition for the current `UObject`
    fn get_uclass(&self, process: &Process, module: &Module) -> Option<UClass> {
        match process.read_pointer(
            self.object + module.offsets.uobject_class,
            module.pointer_size,
        ) {
            Ok(Address::NULL) | Err(_) => None,
            Ok(val) => Some(UClass { class: val }),
        }
    }

    /// Tries to find a field with the specified name in the current UObject and returns
    /// the offset of the field from the start of an instance of the class.
    pub fn get_field_offset(
        &self,
        process: &Process,
        module: &Module,
        field_name: &str,
    ) -> Option<u32> {
        self.get_uclass(process, module)?
            .get_field_offset(process, module, field_name)
    }
}

/// An UClass / UStruct is the object class relative to a specific UObject.
/// It essentially represents the class definition for any given UObject,
/// containing information about its properties, parent and children classes,
/// and much more.
///
/// It's always referred by an UObject and it's used for recover data about
/// its properties and offsets.
///
// Source: https://github.com/bl-sdk/unrealsdk/blob/master/src/unrealsdk/unreal/classes/ustruct.h
#[derive(Copy, Clone)]
struct UClass {
    class: Address,
}

impl UClass {
    fn properties<'a>(
        &'a self,
        process: &'a Process,
        module: &'a Module,
    ) -> impl FusedIterator<Item = UProperty> + '_ {
        // Logic: properties are contained in a linked list that can be accessed directly
        // through the `property_link` field, from the most derived to the least derived class.
        // Source: https://gist.github.com/apple1417/b23f91f7a9e3b834d6d052d35a0010ff#object-structure
        //
        // However, if you are in a class with no additional fields other than the ones it inherits from,
        // `property_link` results in a null pointer. In this case, we access the parent class
        // through the `super_field` offset.
        let mut current_property = {
            let mut val = None;
            let mut current_class = *self;

            while val.is_none() {
                match process.read_pointer(
                    current_class.class + module.offsets.uclass_property_link,
                    module.pointer_size,
                ) {
                    Ok(Address::NULL) => match process.read_pointer(
                        current_class.class + module.offsets.uclass_super_field,
                        module.pointer_size,
                    ) {
                        Ok(Address::NULL) | Err(_) => break,
                        Ok(super_field) => {
                            current_class = UClass { class: super_field };
                        }
                    },
                    Ok(current_property_address) => {
                        val = Some(UProperty {
                            property: current_property_address,
                        });
                    }
                    _ => break,
                }
            }

            val
        };

        iter::from_fn(move || match current_property {
            Some(prop) => match process.read_pointer(
                prop.property + module.offsets.uproperty_property_link_next,
                module.pointer_size,
            ) {
                Ok(val) => {
                    current_property = match val {
                        Address::NULL => None,
                        _ => Some(UProperty { property: val }),
                    };
                    Some(prop)
                }
                _ => None,
            },
            _ => None,
        })
        .fuse()
    }

    /// Returns the offset for the specified named property.
    /// Returns `None` on case of failure.
    fn get_field_offset(
        &self,
        process: &Process,
        module: &Module,
        field_name: &str,
    ) -> Option<u32> {
        self.properties(process, module)
            .find(|field| {
                field
                    .get_fname::<CSTR>(process, module)
                    .is_some_and(|name| name.matches(field_name))
            })?
            .get_offset(process, module)
    }
}

/// Definition for a property used in a certain UClass.
///
/// Used mostly just to recover field names and offsets.
// Source: https://github.com/bl-sdk/unrealsdk/blob/master/src/unrealsdk/unreal/classes/uproperty.h
#[derive(Copy, Clone)]
struct UProperty {
    property: Address,
}

impl UProperty {
    fn get_fname<const N: usize>(
        &self,
        process: &Process,
        module: &Module,
    ) -> Option<ArrayCString<N>> {
        let [name_offset, chunk_offset] = process
            .read::<[u16; 2]>(self.property + module.offsets.uproperty_fname)
            .ok()?;

        let addr = process
            .read_pointer(
                module.fname_base + module.size_of_ptr().wrapping_mul(chunk_offset as u64 + 2),
                module.pointer_size,
            )
            .ok()?
            + (name_offset as u64).wrapping_mul(size_of::<u16>() as u64);

        let string_size = process
            .read::<u16>(addr)
            .ok()?
            .checked_shr(6)
            .unwrap_or_default() as usize;

        let mut string = process
            .read::<ArrayCString<N>>(addr + size_of::<u16>() as u64)
            .ok()?;
        string.set_len(string_size);

        Some(string)
    }

    fn get_offset(&self, process: &Process, module: &Module) -> Option<u32> {
        process
            .read(self.property + module.offsets.uproperty_offset_internal)
            .ok()
    }
}

/// An implementation for automatic pointer path resolution
#[derive(Clone)]
pub struct UnrealPointer<const CAP: usize> {
    cache: RefCell<UnrealPointerCache<CAP>>,
    base_address: Address,
    fields: [&'static str; CAP],
    depth: usize,
}

#[derive(Clone, Copy)]
struct UnrealPointerCache<const CAP: usize> {
    offsets: [u64; CAP],
    resolved_offsets: usize,
}

impl<const CAP: usize> UnrealPointer<CAP> {
    /// Creates a new instance of the Pointer struct
    ///
    /// `CAP` should be higher or equal to the number of offsets defined in `fields`.
    ///
    /// If a higher number of offsets is provided, the pointer path will be truncated
    /// according to the value of `CAP`.
    pub fn new(base_address: Address, fields: &[&'static str]) -> Self {
        let this_fields: [&str; CAP] = {
            let mut iter = fields.iter();
            array::from_fn(|_| iter.next().copied().unwrap_or_default())
        };

        let cache = RefCell::new(UnrealPointerCache {
            offsets: [u64::default(); CAP],
            resolved_offsets: usize::default(),
        });

        Self {
            cache,
            base_address,
            fields: this_fields,
            depth: fields.len().min(CAP),
        }
    }

    /// Tries to resolve the pointer path
    fn find_offsets(&self, process: &Process, module: &Module) -> Option<()> {
        let mut cache = self.cache.borrow_mut();

        // If the pointer path has already been found, there's no need to continue
        if cache.resolved_offsets == self.depth {
            return Some(());
        }

        // If we already resolved some offsets, we need to traverse them again starting from the base address
        // (usually GWorld of GEngine) in order to recalculate the address of the farthest UObject we can reach.
        // If no offsets have been resolved yet, we just need to read the base address instead.
        let mut current_uobject = UObject {
            object: match cache.resolved_offsets {
                0 => process
                    .read_pointer(self.base_address, module.pointer_size)
                    .ok()?,
                x => {
                    let mut addr = process
                        .read_pointer(self.base_address, module.pointer_size)
                        .ok()?;
                    for &i in &cache.offsets[..x] {
                        addr = process.read_pointer(addr + i, module.pointer_size).ok()?;
                    }
                    addr
                }
            },
        };

        for i in cache.resolved_offsets..self.depth {
            let offset_from_string = match self.fields[i].strip_prefix("0x") {
                Some(rem) => u32::from_str_radix(rem, 16).ok(),
                _ => self.fields[i].parse().ok(),
            };

            let current_offset = match offset_from_string {
                Some(offset) => offset as u64,
                _ => current_uobject.get_field_offset(process, module, self.fields[i])? as u64,
            };

            cache.offsets[i] = current_offset;
            cache.resolved_offsets += 1;

            current_uobject = UObject {
                object: process
                    .read_pointer(current_uobject.object + current_offset, module.pointer_size)
                    .ok()?,
            };
        }
        Some(())
    }

    /// Dereferences the pointer path, returning the value stored at the final memory address
    pub fn deref<T: CheckedBitPattern>(&self, process: &Process, module: &Module) -> Option<T> {
        self.find_offsets(process, module)?;
        let cache = self.cache.borrow();
        process
            .read_pointer_path(
                process
                    .read_pointer(self.base_address, module.pointer_size)
                    .ok()?,
                module.pointer_size,
                &cache.offsets[..self.depth],
            )
            .ok()
    }
}

struct Offsets {
    uobject_class: u8,
    uclass_super_field: u8,
    uclass_property_link: u8,
    uproperty_fname: u8,
    uproperty_offset_internal: u8,
    uproperty_property_link_next: u8,
}

impl Offsets {
    const fn new() -> &'static Self {
        &Self {
            uobject_class: 0x10,
            uclass_super_field: 0x40,
            uclass_property_link: 0x50,
            uproperty_fname: 0x28,
            uproperty_offset_internal: 0x4C,
            uproperty_property_link_next: 0x58,
        }
    }
}
