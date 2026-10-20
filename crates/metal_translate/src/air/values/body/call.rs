use super::*;

impl BodyLowering<'_> {
    pub(super) fn call(
        &mut self,
        explicit_signature: Option<&u64>,
        callee: &u64,
        args: &[u64],
        record: &llvm_bitcode::bitcode::Record,
        fields: &[u64],
    ) -> Result<Lowered, TranslateError> {
        let (ty, kind) = {
            let callee = self.table.operand_id(*callee)?;
            let signature = match self.types[self.table.values[callee].ty] {
                DecodedType::Pointer { pointee, .. } => pointee,
                _ => self.table.values[callee].ty,
            };
            if let Some(explicit) = explicit_signature
                && type_reference(Some(explicit), self.types.len())? != signature
            {
                return Err(semantic(
                    "LLVM call's explicit type does not match its target",
                ));
            }
            let DecodedType::Function { result, parameters } = &self.types[signature] else {
                return Err(semantic("LLVM call target is not a function"));
            };
            let name = self
                .names
                .get(&callee)
                .ok_or_else(|| semantic("LLVM call target has no name"))?
                .clone();
            // `llvm.memcpy` declares byte pointers, so its
            // arguments never match the declaration by type. Metal
            // reaches it by assigning one whole structure, which
            // is a load and a store of that structure.
            if name.starts_with("llvm.memcpy.") {
                // Older LLVM spells an alignment between the
                // length and the volatile flag; both forms end
                // with markers this lowering does not need.
                let [destination, source, length, ..] = args else {
                    return Err(semantic("llvm.memcpy has unexpected operands"));
                };
                let destination_id = self.table.operand_id(*destination)?;
                let source_id = self.table.operand_id(*source)?;
                let (into, destination) =
                    self.copied_pointer(*destination, destination_id, record, fields)?;
                let (from, source) = self.copied_pointer(*source, source_id, record, fields)?;
                if into != from {
                    return Err(semantic("llvm.memcpy copies between two different types"));
                }
                let copied = ir_type(into, self.types)?;
                let (size, _) = layout(&copied)?;
                let bytes = self
                    .table
                    .integer(self.table.operand_id(*length)?, self.types)?;
                if bytes != u64::from(size) {
                    return Err(semantic(
                        "llvm.memcpy length is not one whole value of the copied type",
                    ));
                }
                let loaded = emit(
                    &mut self.function,
                    &mut self.instructions,
                    copied,
                    InstructionKind::Load { pointer: source },
                );
                self.instructions.push(Instruction {
                    result: None,
                    kind: InstructionKind::Store {
                        pointer: destination,
                        value: loaded,
                    },
                });
                return Ok(Lowered::Done);
            }
            // Metal reaches `llvm.memset` by initialising a local
            // array to zero, which is a store of a zero value.
            // Any other fill pattern would have to be spelled
            // byte by byte, which logical addressing cannot do.
            if name.starts_with("llvm.memset.") {
                let [destination, fill, length, ..] = args else {
                    return Err(semantic("llvm.memset has unexpected operands"));
                };
                let destination_id = self.table.operand_id(*destination)?;
                let (into, destination) = match self.aliases.get(&destination_id) {
                    Some(alias) => (alias.pointee, alias.operand.clone()),
                    None => {
                        let (ty, value) = self.operand(*destination, None, record, fields)?;
                        match self.types[ty] {
                            DecodedType::Pointer { pointee, .. } => (pointee, value),
                            _ => {
                                return Err(semantic("llvm.memset destination is not a pointer"));
                            }
                        }
                    }
                };
                if self
                    .table
                    .integer(self.table.operand_id(*fill)?, self.types)?
                    != 0
                {
                    return Err(semantic("llvm.memset fills with a nonzero byte"));
                }
                let cleared = ir_type(into, self.types)?;
                let (size, _) = layout(&cleared)?;
                if self
                    .table
                    .integer(self.table.operand_id(*length)?, self.types)?
                    != u64::from(size)
                {
                    return Err(semantic(
                        "llvm.memset length is not one whole value of the cleared type",
                    ));
                }
                self.instructions.push(Instruction {
                    result: None,
                    kind: InstructionKind::Store {
                        pointer: destination,
                        value: zero_composite(&cleared)?,
                    },
                });
                return Ok(Lowered::Done);
            }
            if args.len() != parameters.len() {
                return Err(semantic("LLVM call arity mismatch"));
            }
            let mut actual = Vec::new();
            for (&arg, &expected) in args.iter().zip(parameters) {
                // Fixed call arguments get their type from the signature,
                // including values defined in later blocks of the bitcode.
                let (ty, value) = self.operand(arg, Some(&(expected as u64)), record, fields)?;
                if ty != expected {
                    return Err(semantic("LLVM call argument type mismatch"));
                }
                actual.push(value);
            }
            // A lifetime marker bounds a local's live range for
            // LLVM's own optimizers. SPIR-V has no such thing, and
            // the variable is live for the whole function there.
            if name.starts_with("llvm.lifetime.") {
                return Ok(Lowered::Done);
            }
            let kind = if name.starts_with("air.normalize_function_constant_predicate.") {
                // A static initializer turns a function-constant
                // predicate into the one or zero a gated binding
                // is tested against. On an unsigned integer that
                // is exactly a clamp to [0, 1].
                // FIXME (ACCURACY): the meaning is read from the
                // intrinsic's name; no Apple library in reach
                // calls it with anything but zero or one.
                let [value] = actual.as_slice() else {
                    return Err(semantic(format!("LLVM intrinsic {name} takes one operand")));
                };
                let ty = ir_type(*result, self.types)?;
                let bound = |value| Operand::Integer {
                    ty: ty.clone(),
                    value,
                };
                InstructionKind::IntegerClamp {
                    signed: false,
                    value: value.clone(),
                    min: bound(0),
                    max: bound(1),
                }
            } else {
                known_helper(&name, &actual).unwrap_or_else(|| air_intrinsic(&name, &actual))?
            };
            if matches!(self.types[*result], DecodedType::Void) {
                self.instructions.push(Instruction { result: None, kind });
                return Ok(Lowered::Done);
            }
            (*result, kind)
        };
        Ok(Lowered::Value(ty, kind))
    }

    /// The pointee type and address of one `llvm.memcpy` operand.
    ///
    /// A widening bitcast records the type its pointer really addresses; the
    /// copy is of that, not of what the cast claimed.
    /// The pointee type and address of one `llvm.memcpy` operand.
    ///
    /// A widening bitcast records the type its pointer really addresses; the
    /// copy is of that, not of what the cast claimed.
    fn copied_pointer(
        &mut self,
        encoded: u64,
        id: usize,
        record: &llvm_bitcode::bitcode::Record,
        fields: &[u64],
    ) -> Result<(usize, Operand), TranslateError> {
        if let Some(alias) = self.aliases.get(&id) {
            return Ok((alias.pointee, alias.operand.clone()));
        }
        let (ty, value) = self.operand(encoded, None, record, fields)?;
        match self.types[ty] {
            DecodedType::Pointer { pointee, .. } => Ok((pointee, value)),
            _ => Err(semantic("llvm.memcpy operand is not a pointer")),
        }
    }
}
