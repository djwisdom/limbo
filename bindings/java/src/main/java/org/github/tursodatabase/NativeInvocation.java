package org.github.tursodatabase;


import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Annotation to mark methods that are called by native functions.
 */
@Retention(RetentionPolicy.SOURCE)
@Target(ElementType.METHOD)
public @interface NativeInvocation {
}
