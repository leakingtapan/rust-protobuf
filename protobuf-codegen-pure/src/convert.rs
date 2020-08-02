//! Convert parser model to rust-protobuf model

use std::iter;
use std::path::Path;

use crate::model;

use protobuf;
use protobuf::descriptor::field_descriptor_proto;
use protobuf::json::json_name;
use protobuf::prelude::*;
use protobuf::Message;

use crate::model::FieldOrOneOf;
use crate::model::ProtobufConstant;
use crate::protobuf_codegen::case_convert::camel_case;
use crate::protobuf_codegen::ProtobufAbsolutePath;
use crate::protobuf_codegen::ProtobufIdent;
use crate::protobuf_codegen::ProtobufRelativePath;
use protobuf::reflect::RuntimeTypeBox;
use protobuf::text_format::lexer::StrLitDecodeError;
use protobuf::text_format::quote_bytes_to;

#[derive(Debug)]
pub enum ConvertError {
    UnsupportedOption(String),
    ExtensionNotFound(String),
    WrongExtensionType(String, &'static str),
    UnsupportedExtensionType(String, String, String),
    StrLitDecodeError(StrLitDecodeError),
    DefaultValueIsNotStringLiteral,
    WrongOptionType,
    InconvertibleValue(RuntimeTypeBox, ProtobufConstant),
}

impl From<StrLitDecodeError> for ConvertError {
    fn from(e: StrLitDecodeError) -> Self {
        ConvertError::StrLitDecodeError(e)
    }
}

pub type ConvertResult<T> = Result<T, ConvertError>;

trait ProtobufOptions {
    fn by_name(&self, name: &str) -> Option<&model::ProtobufConstant>;

    fn by_name_bool(&self, name: &str) -> ConvertResult<Option<bool>> {
        match self.by_name(name) {
            Some(model::ProtobufConstant::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(ConvertError::WrongOptionType),
            None => Ok(None),
        }
    }

    fn by_name_string(&self, name: &str) -> ConvertResult<Option<String>> {
        match self.by_name(name) {
            Some(model::ProtobufConstant::String(s)) => s
                .decode_utf8()
                .map(Some)
                .map_err(ConvertError::StrLitDecodeError),
            Some(_) => Err(ConvertError::WrongOptionType),
            None => Ok(None),
        }
    }
}

impl<'a> ProtobufOptions for &'a [model::ProtobufOption] {
    fn by_name(&self, name: &str) -> Option<&model::ProtobufConstant> {
        let option_name = name;
        for model::ProtobufOption { name, value } in *self {
            if name == option_name {
                return Some(&value);
            }
        }
        None
    }
}

enum MessageOrEnum {
    Message,
    Enum,
}

impl MessageOrEnum {
    fn descriptor_type(&self) -> protobuf::descriptor::field_descriptor_proto::Type {
        match *self {
            MessageOrEnum::Message => {
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_MESSAGE
            }
            MessageOrEnum::Enum => protobuf::descriptor::field_descriptor_proto::Type::TYPE_ENUM,
        }
    }
}

enum LookupScope<'a> {
    File(&'a model::FileDescriptor),
    Message(&'a model::Message),
}

impl<'a> LookupScope<'a> {
    fn messages(&self) -> &[model::Message] {
        match self {
            &LookupScope::File(file) => &file.messages,
            &LookupScope::Message(messasge) => &messasge.messages,
        }
    }

    fn find_message(&self, simple_name: &ProtobufIdent) -> Option<&model::Message> {
        self.messages()
            .into_iter()
            .find(|m| m.name == simple_name.get())
    }

    fn enums(&self) -> &[model::Enumeration] {
        match self {
            &LookupScope::File(file) => &file.enums,
            &LookupScope::Message(messasge) => &messasge.enums,
        }
    }

    fn members(&self) -> Vec<(ProtobufIdent, MessageOrEnum)> {
        let mut r = Vec::new();
        r.extend(
            self.enums()
                .into_iter()
                .map(|e| (ProtobufIdent::from(&e.name[..]), MessageOrEnum::Enum)),
        );
        r.extend(
            self.messages()
                .into_iter()
                .map(|e| (ProtobufIdent::from(&e.name[..]), MessageOrEnum::Message)),
        );
        r
    }

    fn find_member(&self, simple_name: &ProtobufIdent) -> Option<MessageOrEnum> {
        self.members()
            .into_iter()
            .filter_map(|(member_name, message_or_enum)| {
                if &member_name == simple_name {
                    Some(message_or_enum)
                } else {
                    None
                }
            })
            .next()
    }

    fn resolve_message_or_enum(
        &self,
        current_path: &ProtobufAbsolutePath,
        path: &ProtobufRelativePath,
    ) -> Option<(ProtobufAbsolutePath, MessageOrEnum)> {
        let (first, rem) = match path.split_first_rem() {
            Some(x) => x,
            None => return None,
        };

        if rem.is_empty() {
            match self.find_member(&first) {
                Some(message_or_enum) => {
                    let mut result_path = current_path.clone();
                    result_path.push_simple(first);
                    Some((result_path, message_or_enum))
                }
                None => None,
            }
        } else {
            match self.find_message(&first) {
                Some(message) => {
                    let mut message_path = current_path.clone();
                    message_path.push_simple(ProtobufIdent::from(message.name.clone()));
                    let message_scope = LookupScope::Message(message);
                    message_scope.resolve_message_or_enum(&message_path, &rem)
                }
                None => None,
            }
        }
    }
}

struct Resolver<'a> {
    current_file: &'a model::FileDescriptor,
    deps: &'a [model::FileDescriptor],
}

impl<'a> Resolver<'a> {
    fn map_entry_name_for_field_name(field_name: &str) -> ProtobufIdent {
        // Field name and message name must match, otherwise
        // Google's validation fails.
        // https://git.io/JeOvF
        ProtobufIdent::from(format!("{}Entry", camel_case(field_name)))
    }

    fn map_entry_field(
        &self,
        name: &str,
        number: i32,
        field_type: &model::FieldType,
        path_in_file: &ProtobufRelativePath,
    ) -> protobuf::descriptor::FieldDescriptorProto {
        // should be consisent with DescriptorBuilder::ValidateMapEntry

        let mut output = protobuf::descriptor::FieldDescriptorProto::new();

        output.set_name(name.to_owned());
        output.set_number(number);

        let (t, t_name) = self.field_type(name, field_type, path_in_file);
        output.set_field_type(t);
        if let Some(t_name) = t_name {
            output.set_type_name(t_name.path);
        }

        output.set_label(field_descriptor_proto::Label::LABEL_OPTIONAL);

        output.set_json_name(json_name(&name));

        output
    }

    fn map_entry_message(
        &self,
        field_name: &str,
        key: &model::FieldType,
        value: &model::FieldType,
        path_in_file: &ProtobufRelativePath,
    ) -> ConvertResult<protobuf::descriptor::DescriptorProto> {
        let mut output = protobuf::descriptor::DescriptorProto::new();

        output.options.mut_message().set_map_entry(true);
        output.set_name(Resolver::map_entry_name_for_field_name(field_name).into_string());
        output
            .field
            .push(self.map_entry_field("key", 1, key, path_in_file));
        output
            .field
            .push(self.map_entry_field("value", 2, value, path_in_file));

        Ok(output)
    }

    fn group_message(
        &self,
        name: &str,
        fields: &[model::Field],
        path_in_file: &ProtobufRelativePath,
    ) -> ConvertResult<protobuf::descriptor::DescriptorProto> {
        let mut output = protobuf::descriptor::DescriptorProto::new();

        output.set_name(name.to_owned());

        for f in fields {
            output.field.push(self.field(f, None, path_in_file)?);
        }

        Ok(output)
    }

    fn message_options(
        &self,
        input: &[model::ProtobufOption],
    ) -> ConvertResult<protobuf::descriptor::MessageOptions> {
        self.custom_options(input)
    }

    fn message(
        &self,
        input: &model::Message,
        path_in_file: &ProtobufRelativePath,
    ) -> ConvertResult<protobuf::descriptor::DescriptorProto> {
        let nested_path_in_file = path_in_file.append_ident(&ProtobufIdent::from(&input.name[..]));

        let mut output = protobuf::descriptor::DescriptorProto::new();
        output.set_name(input.name.clone());

        let mut nested_messages = protobuf::RepeatedField::new();

        for m in &input.messages {
            nested_messages.push(self.message(m, &nested_path_in_file)?);
        }

        for f in input.regular_fields_including_in_oneofs() {
            match &f.typ {
                model::FieldType::Map(t) => {
                    nested_messages.push(self.map_entry_message(
                        &f.name,
                        &t.0,
                        &t.1,
                        path_in_file,
                    )?);
                }
                model::FieldType::Group { name, fields } => {
                    nested_messages.push(self.group_message(name, fields, &nested_path_in_file)?);
                }
                _ => (),
            }
        }

        output.nested_type = nested_messages;

        output.enum_type = input
            .enums
            .iter()
            .map(|e| self.enumeration(e))
            .collect::<Result<_, _>>()?;

        {
            let mut fields = protobuf::RepeatedField::new();

            for fo in &input.fields {
                match fo {
                    FieldOrOneOf::Field(f) => {
                        fields.push(self.field(f, None, &nested_path_in_file)?);
                    }
                    FieldOrOneOf::OneOf(o) => {
                        let oneof_index = output.oneof_decl.len();
                        for f in &o.fields {
                            fields.push(self.field(
                                f,
                                Some(oneof_index as i32),
                                &nested_path_in_file,
                            )?);
                        }
                        output.oneof_decl.push(self.oneof(o));
                    }
                }
            }

            output.field = fields;
        }

        output
            .options
            .set_message(self.message_options(&input.options)?);

        for ext in &input.extensions {
            let mut extension_range = protobuf::descriptor::descriptor_proto::ExtensionRange::new();
            extension_range.set_start(ext.from);
            // TODO: is end inclusive or exclusive?
            extension_range.set_end(ext.to);
            output.extension_range.push(extension_range);
        }

        Ok(output)
    }

    fn custom_options<M>(&self, input: &[model::ProtobufOption]) -> ConvertResult<M>
    where
        M: Message,
    {
        let mut options = M::new();
        let extendee = M::descriptor_static().full_name();

        for option in input {
            if !option.name.starts_with('(') {
                if let Some(field) = M::descriptor_static().get_field_by_name(&option.name) {
                    if field.is_repeated() || field.is_map() {
                        continue;
                    }

                    field.set_singular_field(
                        &mut options,
                        option
                            .value
                            .as_type(field.singular_runtime_type().to_box())?,
                    );
                }
                continue;
            }

            let extension = match self.find_extension(&option.name) {
                Ok(e) => e,
                // TODO: return error
                Err(_) => continue,
            };
            if extension.extendee != extendee {
                return Err(ConvertError::WrongExtensionType(
                    option.name.clone(),
                    extendee,
                ));
            }

            let value = match Resolver::option_value_to_unknown_value(
                &option.value,
                &extension.field.typ,
                &option.name,
            ) {
                Ok(value) => value,
                Err(_) => {
                    // TODO: return error
                    continue;
                }
            };

            options
                .mut_unknown_fields()
                .add_value(extension.field.number as u32, value);
        }
        Ok(options)
    }

    fn field_options(
        &self,
        input: &[model::ProtobufOption],
    ) -> ConvertResult<protobuf::descriptor::FieldOptions> {
        self.custom_options(input)
    }

    fn field(
        &self,
        input: &model::Field,
        oneof_index: Option<i32>,
        path_in_file: &ProtobufRelativePath,
    ) -> ConvertResult<protobuf::descriptor::FieldDescriptorProto> {
        let mut output = protobuf::descriptor::FieldDescriptorProto::new();
        output.set_name(input.name.clone());

        if let model::FieldType::Map(..) = input.typ {
            output.set_label(protobuf::descriptor::field_descriptor_proto::Label::LABEL_REPEATED);
        } else {
            output.set_label(label(input.rule));
        }

        let (t, t_name) = self.field_type(&input.name, &input.typ, path_in_file);
        output.set_field_type(t);
        if let Some(t_name) = t_name {
            output.set_type_name(t_name.path);
        }

        output.set_number(input.number);
        if let Some(ref default) = input.options.as_slice().by_name("default") {
            let default = match output.get_field_type() {
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_STRING => {
                    if let &model::ProtobufConstant::String(ref s) = default {
                        s.decode_utf8()?
                    } else {
                        return Err(ConvertError::DefaultValueIsNotStringLiteral);
                    }
                }
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_BYTES => {
                    if let &model::ProtobufConstant::String(ref s) = default {
                        let mut buf = String::new();
                        quote_bytes_to(&s.decode_bytes()?, &mut buf);
                        buf
                    } else {
                        return Err(ConvertError::DefaultValueIsNotStringLiteral);
                    }
                }
                _ => default.format(),
            };
            output.set_default_value(default);
        }

        output
            .options
            .set_message(self.field_options(&input.options)?);

        if let Some(oneof_index) = oneof_index {
            output.set_oneof_index(oneof_index);
        }

        if let Some(json_name) = input.options.as_slice().by_name_string("json_name")? {
            output.set_json_name(json_name);
        } else {
            output.set_json_name(json_name(&input.name));
        }

        Ok(output)
    }

    fn all_files(&self) -> Vec<&model::FileDescriptor> {
        iter::once(self.current_file).chain(self.deps).collect()
    }

    fn package_files(&self, package: Option<&str>) -> Vec<&model::FileDescriptor> {
        self.all_files()
            .into_iter()
            .filter(|f| f.package.as_deref() == package)
            .collect()
    }

    fn current_file_package_files(&self) -> Vec<&model::FileDescriptor> {
        self.package_files(self.current_file.package.as_deref())
    }

    fn resolve_message_or_enum(
        &self,
        name: &str,
        path_in_file: &ProtobufRelativePath,
    ) -> (ProtobufAbsolutePath, MessageOrEnum) {
        // find message or enum in current package
        if !name.starts_with(".") {
            for p in path_in_file.self_and_parents() {
                let relative_path_with_name = p.clone();
                let relative_path_with_name =
                    relative_path_with_name.append(&ProtobufRelativePath::from(name));
                for file in self.current_file_package_files() {
                    if let Some((n, t)) = LookupScope::File(file).resolve_message_or_enum(
                        &ProtobufAbsolutePath::from_package_path(file.package.as_deref()),
                        &relative_path_with_name,
                    ) {
                        return (n, t);
                    }
                }
            }
        }

        // find message or enum in root package
        {
            let absolute_path = ProtobufAbsolutePath::from_path_maybe_dot(name);
            for file in self.all_files() {
                let file_package = ProtobufAbsolutePath::from_package_path(file.package.as_deref());
                if let Some(relative) = absolute_path.remove_prefix(&file_package) {
                    if let Some((n, t)) =
                        LookupScope::File(file).resolve_message_or_enum(&file_package, &relative)
                    {
                        return (n, t);
                    }
                }
            }
        }

        panic!(
            "couldn't find message or enum {} when parsing {}",
            name,
            self.current_file.package.as_deref().unwrap_or("")
        );
    }

    fn field_type(
        &self,
        name: &str,
        input: &model::FieldType,
        path_in_file: &ProtobufRelativePath,
    ) -> (
        protobuf::descriptor::field_descriptor_proto::Type,
        Option<ProtobufAbsolutePath>,
    ) {
        match *input {
            model::FieldType::Bool => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_BOOL,
                None,
            ),
            model::FieldType::Int32 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_INT32,
                None,
            ),
            model::FieldType::Int64 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_INT64,
                None,
            ),
            model::FieldType::Uint32 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_UINT32,
                None,
            ),
            model::FieldType::Uint64 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_UINT64,
                None,
            ),
            model::FieldType::Sint32 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_SINT32,
                None,
            ),
            model::FieldType::Sint64 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_SINT64,
                None,
            ),
            model::FieldType::Fixed32 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_FIXED32,
                None,
            ),
            model::FieldType::Fixed64 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_FIXED64,
                None,
            ),
            model::FieldType::Sfixed32 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_SFIXED32,
                None,
            ),
            model::FieldType::Sfixed64 => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_SFIXED64,
                None,
            ),
            model::FieldType::Float => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_FLOAT,
                None,
            ),
            model::FieldType::Double => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_DOUBLE,
                None,
            ),
            model::FieldType::String => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_STRING,
                None,
            ),
            model::FieldType::Bytes => (
                protobuf::descriptor::field_descriptor_proto::Type::TYPE_BYTES,
                None,
            ),
            model::FieldType::MessageOrEnum(ref name) => {
                let (name, me) = self.resolve_message_or_enum(&name, path_in_file);
                (me.descriptor_type(), Some(name))
            }
            model::FieldType::Map(..) => {
                let mut type_name =
                    ProtobufAbsolutePath::from_package_path(self.current_file.package.as_deref());
                type_name.push_relative(path_in_file);
                type_name.push_simple(Resolver::map_entry_name_for_field_name(name));
                (
                    protobuf::descriptor::field_descriptor_proto::Type::TYPE_MESSAGE,
                    Some(type_name),
                )
            }
            model::FieldType::Group { ref name, .. } => {
                let mut type_name =
                    ProtobufAbsolutePath::from_package_path(self.current_file.package.as_deref());
                type_name.push_relative(path_in_file);
                type_name.push_simple(ProtobufIdent::from(name.clone()));
                (
                    protobuf::descriptor::field_descriptor_proto::Type::TYPE_GROUP,
                    Some(type_name),
                )
            }
        }
    }

    fn enum_value(
        &self,
        name: &str,
        number: i32,
    ) -> protobuf::descriptor::EnumValueDescriptorProto {
        let mut output = protobuf::descriptor::EnumValueDescriptorProto::new();
        output.set_name(name.to_owned());
        output.set_number(number);
        output
    }

    fn enum_options(
        &self,
        input: &[model::ProtobufOption],
    ) -> ConvertResult<protobuf::descriptor::EnumOptions> {
        self.custom_options(input)
    }

    fn enumeration(
        &self,
        input: &model::Enumeration,
    ) -> ConvertResult<protobuf::descriptor::EnumDescriptorProto> {
        let mut output = protobuf::descriptor::EnumDescriptorProto::new();
        output.set_name(input.name.clone());
        output.value = input
            .values
            .iter()
            .map(|v| self.enum_value(&v.name, v.number))
            .collect();
        output
            .options
            .set_message(self.enum_options(&input.options)?);
        Ok(output)
    }

    fn oneof(&self, input: &model::OneOf) -> protobuf::descriptor::OneofDescriptorProto {
        let mut output = protobuf::descriptor::OneofDescriptorProto::new();
        output.set_name(input.name.clone());
        output
    }

    fn find_extension_by_path(&self, path: &str) -> ConvertResult<&model::Extension> {
        let (package, name) = match path.rfind('.') {
            Some(dot) => (Some(&path[..dot]), &path[dot + 1..]),
            None => (self.current_file.package.as_deref(), path),
        };

        for file in self.package_files(package) {
            for ext in &file.extensions {
                if ext.field.name == name {
                    return Ok(ext);
                }
            }
        }

        Err(ConvertError::ExtensionNotFound(path.to_owned()))
    }

    fn find_extension(&self, option_name: &str) -> ConvertResult<&model::Extension> {
        if !option_name.starts_with('(') || !option_name.ends_with(')') {
            return Err(ConvertError::UnsupportedOption(option_name.to_owned()));
        }
        let path = &option_name[1..option_name.len() - 1];
        self.find_extension_by_path(path)
    }

    fn option_value_to_unknown_value(
        value: &model::ProtobufConstant,
        field_type: &model::FieldType,
        option_name: &str,
    ) -> ConvertResult<protobuf::UnknownValue> {
        let v = match value {
            &model::ProtobufConstant::Bool(b) => {
                if field_type != &model::FieldType::Bool {
                    Err(())
                } else {
                    Ok(protobuf::UnknownValue::Varint(if b { 1 } else { 0 }))
                }
            }
            // TODO: check overflow
            &model::ProtobufConstant::U64(v) => match field_type {
                &model::FieldType::Fixed64 | model::FieldType::Sfixed64 => {
                    Ok(protobuf::UnknownValue::Fixed64(v))
                }
                &model::FieldType::Fixed32 | model::FieldType::Sfixed32 => {
                    Ok(protobuf::UnknownValue::Fixed32(v as u32))
                }
                &model::FieldType::Int64
                | &model::FieldType::Int32
                | &model::FieldType::Uint64
                | &model::FieldType::Uint32 => Ok(protobuf::UnknownValue::Varint(v)),
                &model::FieldType::Sint64 => Ok(protobuf::UnknownValue::sint64(v as i64)),
                &model::FieldType::Sint32 => Ok(protobuf::UnknownValue::sint32(v as i32)),
                _ => Err(()),
            },
            &model::ProtobufConstant::I64(v) => match field_type {
                &model::FieldType::Fixed64 | model::FieldType::Sfixed64 => {
                    Ok(protobuf::UnknownValue::Fixed64(v as u64))
                }
                &model::FieldType::Fixed32 | model::FieldType::Sfixed32 => {
                    Ok(protobuf::UnknownValue::Fixed32(v as u32))
                }
                &model::FieldType::Int64
                | &model::FieldType::Int32
                | &model::FieldType::Uint64
                | &model::FieldType::Uint32 => Ok(protobuf::UnknownValue::Varint(v as u64)),
                &model::FieldType::Sint64 => Ok(protobuf::UnknownValue::sint64(v as i64)),
                &model::FieldType::Sint32 => Ok(protobuf::UnknownValue::sint32(v as i32)),
                _ => Err(()),
            },
            &model::ProtobufConstant::F64(f) => match field_type {
                &model::FieldType::Float => {
                    Ok(protobuf::UnknownValue::Fixed32((f as f32).to_bits()))
                }
                &model::FieldType::Double => Ok(protobuf::UnknownValue::Fixed64(f.to_bits())),
                _ => Err(()),
            },
            &model::ProtobufConstant::String(ref s) => match field_type {
                &model::FieldType::String => Ok(protobuf::UnknownValue::LengthDelimited(
                    s.decode_utf8()?.into_bytes(),
                )),
                &model::FieldType::Bytes => {
                    Ok(protobuf::UnknownValue::LengthDelimited(s.decode_bytes()?))
                }
                _ => Err(()),
            },
            _ => Err(()),
        };

        v.map_err(|()| {
            ConvertError::UnsupportedExtensionType(
                option_name.to_owned(),
                format!("{:?}", field_type),
                format!("{:?}", value),
            )
        })
    }

    fn file_options(
        &self,
        input: &[model::ProtobufOption],
    ) -> ConvertResult<protobuf::descriptor::FileOptions> {
        self.custom_options(input)
    }

    fn extension(
        &self,
        input: &model::Extension,
    ) -> ConvertResult<(
        protobuf::descriptor::FieldDescriptorProto,
        Vec<protobuf::descriptor::DescriptorProto>,
    )> {
        let relative_path = ProtobufRelativePath::new("".to_owned());
        let mut field = self.field(&input.field, None, &relative_path)?;
        field.set_extendee(
            self.resolve_message_or_enum(&input.extendee, &relative_path)
                .0
                .path,
        );
        // TODO: collect groups
        Ok((field, Vec::new()))
    }
}

fn syntax(input: model::Syntax) -> String {
    match input {
        model::Syntax::Proto2 => "proto2".to_owned(),
        model::Syntax::Proto3 => "proto3".to_owned(),
    }
}

fn label(input: model::Rule) -> protobuf::descriptor::field_descriptor_proto::Label {
    match input {
        model::Rule::Optional => {
            protobuf::descriptor::field_descriptor_proto::Label::LABEL_OPTIONAL
        }
        model::Rule::Required => {
            protobuf::descriptor::field_descriptor_proto::Label::LABEL_REQUIRED
        }
        model::Rule::Repeated => {
            protobuf::descriptor::field_descriptor_proto::Label::LABEL_REPEATED
        }
    }
}

pub fn file_descriptor(
    name: &Path,
    input: &model::FileDescriptor,
    deps: &[model::FileDescriptor],
) -> ConvertResult<protobuf::descriptor::FileDescriptorProto> {
    let name = name.to_str().expect("not a valid UTF-8 name");

    let resolver = Resolver {
        current_file: &input,
        deps,
    };

    let mut output = protobuf::descriptor::FileDescriptorProto::new();
    output.set_name(name.to_owned());
    output.set_syntax(syntax(input.syntax));

    if let Some(package) = &input.package {
        output.set_package(package.clone());
    }

    output.dependency = protobuf::RepeatedField::from_slice(&input.import_paths);

    let mut messages = protobuf::RepeatedField::new();
    for m in &input.messages {
        messages.push(resolver.message(&m, &ProtobufRelativePath::empty())?);
    }
    output.message_type = messages;

    output.enum_type = input
        .enums
        .iter()
        .map(|e| resolver.enumeration(e))
        .collect::<Result<_, _>>()?;

    output
        .options
        .set_message(resolver.file_options(&input.options)?);

    let mut extensions = protobuf::RepeatedField::new();
    for e in &input.extensions {
        let (ext, group_messages) = resolver.extension(e)?;
        extensions.push(ext);
        output.message_type.extend(group_messages);
    }
    output.extension = extensions;

    Ok(output)
}
